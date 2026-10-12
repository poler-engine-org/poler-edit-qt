//! poler-edit :: ядро буфера — Zero-Copy Piece-Table поверх mmap.
//!
//! Модель памяти:
//! * Оригинал файла отображается через mmap и НИКОГДА не копируется в кучу.
//!   Он нарезается на куски-дескрипторы `MAX_PIECE_BYTES` (чистые метаданные,
//!   создание O(file/16MB) без единого чтения данных).
//! * Правки дописываются в append-only буферы (`edit_bufs`) и ссылаются
//!   кусками `Src::Edit`. Исходные байты неизменяемы — классическая piece
//!   table, но с нарезкой оригинала для ленивой адресации строк.
//! * Ленивый SIMD line-index: число `\n` кускa считается при первом
//!   касании (memchr, ~ГБ/с) и кэшируется в самом куске. Префикс
//!   `piece_line_start` достраивается по требованию (вьюпорт, goto, поиск).
//!
//! Инварианты:
//! * `cum[i]` — сумма длин кусков 0..=i (байтовый конец куска i).
//! * `piece_line_start[i]` валиден для i <= known_prefix; элемент
//!   `[pieces.len()]` — номер «строки после последнего \n» (= полных строк).
//! * Число строк документа = 1 + total_nl (пустой файл — 1 пустая строка).

use memmap2::Mmap;
use rayon::prelude::*;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Максимальный кусок оригинала: нарезка чистой метадаты, ленивый подсчёт
/// строк никогда не сканирует больше этого за один шаг отклика.
pub const MAX_PIECE_BYTES: u64 = 16 << 20;

/// Строки длиннее этого усекаются во вьюпорте (защита рендера, как Kate).
pub const VIEWPORT_MAX_LINE_BYTES: usize = 8192;

/// Бюджет метаданных undo-истории (байт на снапшоты кусков).
const UNDO_SNAPSHOT_BUDGET: u64 = 4_000_000;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Src {
    /// Диапазон [start, start+len) в mmap оригинала.
    Orig { start: u64 },
    /// Диапазон [start, start+len) в буфере правок edit_bufs[buf].
    Edit { buf: u32, start: u64 },
}

fn src_shifted(src: Src, delta: u64) -> Src {
    match src {
        Src::Orig { start } => Src::Orig { start: start + delta },
        Src::Edit { buf, start } => Src::Edit { buf, start: start + delta },
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Piece {
    pub src: Src,
    pub len: u32,
    /// Лениво подсчитанное число `\n` (кэш SIMD-скана).
    pub nl: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct BufferStats {
    pub path: PathBuf,
    pub bytes: u64,
    pub pieces: usize,
    /// Число строк, если префикс уже доиндексирован до конца.
    pub lines: Option<u64>,
    /// Байтов, уже покрытых line-index (для прогресс-бара).
    pub indexed_bytes: u64,
    pub edited: bool,
    pub undo_depth: usize,
    pub redo_depth: usize,
}

#[derive(Clone, Debug)]
pub struct ViewportLine {
    pub line: u64,
    pub text: String,
    pub truncated: bool,
}

#[derive(Clone, Debug)]
pub struct SearchHit {
    pub byte: u64,
    pub len: usize,
    pub line: u64,
    /// Столбец в байтах от начала строки.
    pub col: u64,
}

#[derive(Clone, Debug)]
pub struct SearchStats {
    pub hits: Vec<SearchHit>,
    pub truncated: bool,
    pub cancelled: bool,
    pub scanned_bytes: u64,
    pub elapsed: Duration,
}

struct Snapshot {
    pieces: Vec<Piece>,
    piece_line_start: Vec<u64>,
    known_prefix: usize,
}

pub struct PolerBuffer {
    path: PathBuf,
    mmap: Option<Mmap>,
    edit_bufs: Vec<Vec<u8>>,
    pieces: Vec<Piece>,
    cum: Vec<u64>,
    piece_line_start: Vec<u64>,
    known_prefix: usize,
    dirty: bool,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
}

#[inline]
fn count_newlines(bytes: &[u8]) -> usize {
    memchr::memchr_iter(b'\n', bytes).count()
}

impl PolerBuffer {
    /// Открыть файл любого размера: mmap + нарезка метадаты, ноль чтения.
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        Self::open_with_cap(path, MAX_PIECE_BYTES)
    }

    /// То же с кастомным капом куска (тесты упражняют границы кусков).
    pub fn open_with_cap<P: AsRef<Path>>(path: P, cap: u64) -> io::Result<Self> {
        let cap = cap.max(8);
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path)?;
        let meta = file.metadata()?;
        if meta.is_dir() {
            return Err(io::Error::new(io::ErrorKind::IsADirectory, "это каталог"));
        }
        let len = meta.len();
        let (mmap, pieces) = if len == 0 {
            (None, Vec::new())
        } else {
            let mmap = unsafe { Mmap::map(&file)? };
            // Интерактивный доступ случайными окнами: без агрессивного
            // read-ahead ядра. Сканы (поиск/индекс) сами советуют WILLNEED.
            #[cfg(unix)]
            let _ = mmap.advise(memmap2::Advice::Random);
            let mut pieces =
                Vec::with_capacity((len.div_ceil(cap) + 1) as usize);
            let mut off = 0u64;
            while off < len {
                let chunk = (len - off).min(cap) as u32;
                pieces.push(Piece { src: Src::Orig { start: off }, len: chunk, nl: None });
                off += chunk as u64;
            }
            (Some(mmap), pieces)
        };
        let n = pieces.len();
        Ok(Self {
            path,
            mmap,
            edit_bufs: Vec::new(),
            cum: pieces.iter().scan(0u64, |s, p| { *s += p.len as u64; Some(*s) }).collect(),
            piece_line_start: vec![0; n + 1],
            known_prefix: 0,
            pieces,
            dirty: false,
            undo: Vec::new(),
            redo: Vec::new(),
        })
    }

    /// Новый пустой документ (untitled).
    pub fn new_empty<P: AsRef<Path>>(path: P) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            mmap: None,
            edit_bufs: Vec::new(),
            pieces: Vec::new(),
            cum: Vec::new(),
            piece_line_start: vec![0],
            known_prefix: 0,
            dirty: false,
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    #[inline]
    pub fn total_len(&self) -> u64 {
        self.cum.last().copied().unwrap_or(0)
    }

    /// Число кусков piece-table (метаданные; для статистики/тестов).
    pub fn pieces_len(&self) -> usize {
        self.pieces.len()
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn stats(&self) -> BufferStats {
        BufferStats {
            path: self.path.clone(),
            bytes: self.total_len(),
            pieces: self.pieces.len(),
            lines: self.lines_if_known(),
            indexed_bytes: self.indexed_bytes(),
            edited: self.dirty,
            undo_depth: self.undo.len(),
            redo_depth: self.redo.len(),
        }
    }

    /// Полное число строк, если line-index уже дошёл до конца файла.
    pub fn lines_if_known(&self) -> Option<u64> {
        if self.known_prefix >= self.pieces.len() {
            Some(1 + self.piece_line_start[self.pieces.len()])
        } else {
            None
        }
    }

    pub fn indexed_bytes(&self) -> u64 {
        if self.known_prefix == 0 {
            0
        } else {
            self.cum[self.known_prefix - 1]
        }
    }

    // ---------------- внутренние примитивы ----------------

    #[inline]
    fn piece_start(&self, i: usize) -> u64 {
        self.cum[i] - self.pieces[i].len as u64
    }

    #[inline]
    fn piece_bytes(&self, i: usize) -> &[u8] {
        let p = &self.pieces[i];
        match p.src {
            Src::Orig { start } => {
                let m = self.mmap.as_ref().expect("кусок Orig без mmap");
                &m[start as usize..(start + p.len as u64) as usize]
            }
            Src::Edit { buf, start } => {
                let b = &self.edit_bufs[buf as usize];
                &b[start as usize..(start + p.len as u64) as usize]
            }
        }
    }

    #[inline]
    fn byte_at(&self, off: u64) -> u8 {
        let i = self.locate(off);
        let intra = (off - self.piece_start(i)) as usize;
        self.piece_bytes(i)[intra]
    }

    /// Индекс куска, содержащего байтовое смещение off (off < total_len).
    #[inline]
    fn locate(&self, off: u64) -> usize {
        debug_assert!(off < self.total_len());
        match self.cum.binary_search(&off) {
            Ok(i) => i + 1,
            Err(i) => i,
        }
    }

    fn rebuild_cum(&mut self) {
        self.cum.clear();
        self.cum.reserve(self.pieces.len());
        let mut s = 0u64;
        for p in &self.pieces {
            s += p.len as u64;
            self.cum.push(s);
        }
    }

    /// Совет ядру о чтении диапазона оригинала.
    #[cfg(unix)]
    fn advise(&self, start: u64, len: usize, advice: libc::c_int) {
        if let Some(m) = &self.mmap {
            let base = m.as_ptr() as usize;
            let off = start as usize;
            if off < m.len() {
                let l = len.min(m.len() - off);
                if l > 0 {
                    unsafe {
                        libc::madvise(
                            (base + off) as *mut libc::c_void,
                            l,
                            advice,
                        );
                    }
                }
            }
        }
    }

    #[cfg(not(unix))]
    #[allow(unused_variables)]
    fn advise(&self, start: u64, len: usize, advice: i32) {}

    /// Сканирование куска-оригинала: WILLNEED → счёт → DONTNEED
    /// (как poler_disk_harvester: RSS не зависит от размера файла,
    /// страницы возвращаются ядру, page cache сохраняет данные).
    fn scan_orig_piece(&self, i: usize) -> u32 {
        let p = &self.pieces[i];
        if let Src::Orig { start } = p.src {
            #[cfg(unix)]
            self.advise(start, p.len as usize, libc::MADV_SEQUENTIAL);
            let n = count_newlines(self.piece_bytes(i)) as u32;
            #[cfg(unix)]
            self.advise(start, p.len as usize, libc::MADV_DONTNEED);
            n
        } else {
            count_newlines(self.piece_bytes(i)) as u32
        }
    }

    // ---------------- line index (ленивый, SIMD) ----------------

    /// Посчитать `\n` куска i (один раз), продлить префикс строк.
    fn ensure_piece_counted(&mut self, i: usize) -> io::Result<()> {
        if self.pieces[i].nl.is_none() {
            let n = self.scan_orig_piece(i);
            self.pieces[i].nl = Some(n);
        }
        let nl = self.pieces[i].nl.unwrap() as u64;
        self.piece_line_start[i + 1] = self.piece_line_start[i] + nl;
        self.known_prefix = i + 1;
        Ok(())
    }

    /// Пересобрать piece_line_start от валидного якоря `from` вперёд
    /// по уже посчитанным кускам (после splice правок).
    /// Якорь: from ДОЛЖЕН указывать на нетронутый кусок с валидным
    /// line_start (вызов передаёт позицию ПЕРЕД точкой правки).
    fn reline_from(&mut self, from: usize) {
        self.piece_line_start.resize(self.pieces.len() + 1, 0);
        let mut j = from.min(self.known_prefix);
        while j < self.pieces.len() {
            match self.pieces[j].nl {
                Some(nl) => {
                    self.piece_line_start[j + 1] = self.piece_line_start[j] + nl as u64;
                    j += 1;
                }
                None => break,
            }
        }
        self.known_prefix = j;
    }

    /// Номер строки (0-based) для байтового смещения.
    pub fn line_of_offset(&mut self, off: u64) -> io::Result<u64> {
        if self.pieces.is_empty() {
            return Ok(0);
        }
        let total = self.total_len();
        if off >= total {
            // Строка после последнего \n (или EOF): выводим из последнего байта.
            let p = total - 1;
            let l = self.line_of_offset(p)?;
            return Ok(if self.byte_at(p) == b'\n' { l + 1 } else { l });
        }
        let i = self.locate(off);
        while self.known_prefix < i {
            let j = self.known_prefix;
            self.ensure_piece_counted(j)?;
        }
        let intra = (off - self.piece_start(i)) as usize;
        let partial = count_newlines(&self.piece_bytes(i)[..intra]) as u64;
        Ok(self.piece_line_start[i] + partial)
    }

    /// Байтовое смещение начала строки (0-based). Дотягивает line-index.
    ///
    /// Строка L начинается сразу после L-го `\n`, а этот `\n` лежит в
    /// единственном куске i с инвариантом starts[i] < L <= starts[i+1]
    /// (starts[i] — число `\n` в кусках ДО i). binary_search тут
    /// некорректен из-за дублей (нулёвые куски без переводов строки):
    /// используем partition_point — последний кусок с starts[i] < L.
    pub fn offset_of_line(&mut self, line: u64) -> io::Result<u64> {
        if self.pieces.is_empty() || line == 0 {
            return Ok(0);
        }
        // Дотянуть префикс: пока не увидели L переводов строки.
        while self.known_prefix < self.pieces.len()
            && self.piece_line_start[self.known_prefix] < line
        {
            let j = self.known_prefix;
            self.ensure_piece_counted(j)?;
        }
        let kp = self.known_prefix;
        if kp >= self.pieces.len() && line > self.piece_line_start[kp] {
            return Ok(self.total_len()); // за пределами EOF-строки
        }
        // i = последний индекс с starts[i] < line (существует: starts[0]=0<line).
        let starts = &self.piece_line_start[0..=kp];
        let i = starts.partition_point(|&x| x < line) - 1;
        // target = порядковый номер нужного \n внутри куска i (1-based).
        let mut target = line - self.piece_line_start[i];
        let pstart = self.piece_start(i);
        for pos in memchr::memchr_iter(b'\n', self.piece_bytes(i)) {
            target -= 1;
            if target == 0 {
                return Ok(pstart + pos as u64 + 1);
            }
        }
        // Неожиданно мало \n (не должно случаться) — безопасный клэмп.
        Ok(self.total_len())
    }

    /// Полный индекс строк с прогрессом; отмена по флагу.
    /// Подсчёт \n по кускам идёт ПАРАЛЛЕЛЬНО (rayon, как harvester),
    /// затем префикс строк собирается последовательно за O(pieces).
    /// Возвращает (строк, завершён ли до конца).
    pub fn index_all(
        &mut self,
        cancel: &AtomicBool,
        mut progress: impl FnMut(u64, u64),
    ) -> io::Result<(u64, bool)> {
        let total = self.total_len();
        let unknown: Vec<usize> = (0..self.pieces.len())
            .filter(|&i| self.pieces[i].nl.is_none())
            .collect();
        if !unknown.is_empty() {
            let mut done: u64 = self
                .pieces
                .iter()
                .filter(|p| p.nl.is_some())
                .map(|p| p.len as u64)
                .sum();
            // Батчами: после каждого — прогресс и проверка отмены.
            const BATCH: usize = 256;
            let mut idx = 0usize;
            let mut cancelled = false;
            while idx < unknown.len() {
                if cancel.load(Ordering::Relaxed) {
                    cancelled = true;
                    break;
                }
                let hi = (idx + BATCH).min(unknown.len());
                let batch: Vec<usize> = unknown[idx..hi].to_vec();
                let pieces = &self.pieces;
                let mmap = self.mmap.as_ref();
                let edit_bufs = &self.edit_bufs;
                let results: Vec<Option<u32>> = batch
                    .into_par_iter()
                    .map(|i| {
                        if cancel.load(Ordering::Relaxed) {
                            return None;
                        }
                        let p = &pieces[i];
                        let bytes: &[u8] = match p.src {
                            Src::Orig { start } => {
                                let m = mmap.as_ref().expect("кусок Orig без mmap");
                                #[cfg(unix)]
                                unsafe {
                                    libc::madvise(
                                        m.as_ptr().add(start as usize) as *mut libc::c_void,
                                        p.len as usize,
                                        libc::MADV_SEQUENTIAL,
                                    );
                                }
                                &m[start as usize..(start + p.len as u64) as usize]
                            }
                            Src::Edit { buf, start } => {
                                let b = &edit_bufs[buf as usize];
                                &b[start as usize..(start + p.len as u64) as usize]
                            }
                        };
                        let n = count_newlines(bytes) as u32;
                        if let (Some(m), Src::Orig { start }) = (mmap, p.src) {
                            #[cfg(unix)]
                            unsafe {
                                libc::madvise(
                                    m.as_ptr().add(start as usize) as *mut libc::c_void,
                                    p.len as usize,
                                    libc::MADV_DONTNEED,
                                );
                            }
                            #[cfg(not(unix))]
                            let _ = (m, start);
                        }
                        Some(n)
                    })
                    .collect();
                let mut any_skipped = false;
                for (k, r) in results.into_iter().enumerate() {
                    match r {
                        Some(n) => {
                            let i = unknown[idx + k];
                            self.pieces[i].nl = Some(n);
                            done += self.pieces[i].len as u64;
                        }
                        None => any_skipped = true,
                    }
                }
                idx = hi;
                progress(done.min(total), total);
                if any_skipped {
                    cancelled = true;
                    break;
                }
            }
            // Префикс строк: пересобрать с нуля по кэшированным nl.
            self.reline_from(0);
            if cancelled {
                return Ok((0, false));
            }
        } else {
            self.reline_from(0);
        }
        progress(total, total);
        Ok((1 + self.piece_line_start[self.pieces.len()], true))
    }

    // ---------------- чтение ----------------

    /// Позиция следующего `\n` начиная с off (включая off), либо None.
    fn next_newline_from(&self, mut off: u64) -> Option<u64> {
        let total = self.total_len();
        while off < total {
            let i = self.locate(off);
            let pstart = self.piece_start(i);
            let intra = (off - pstart) as usize;
            let pb = self.piece_bytes(i);
            if let Some(p) = memchr::memchr(b'\n', &pb[intra..]) {
                return Some(pstart + (intra + p) as u64);
            }
            off = self.cum[i];
        }
        None
    }

    pub fn read_range(&self, start: u64, len: usize) -> Vec<u8> {
        let total = self.total_len();
        if start >= total || len == 0 {
            return Vec::new();
        }
        let end = (start + len as u64).min(total);
        let mut out = Vec::with_capacity((end - start) as usize);
        let mut off = start;
        while off < end {
            let i = self.locate(off);
            let pstart = self.piece_start(i);
            let intra = (off - pstart) as usize;
            let avail = self.pieces[i].len as usize - intra;
            let take = avail.min((end - off) as usize);
            out.extend_from_slice(&self.piece_bytes(i)[intra..intra + take]);
            off += take as u64;
        }
        out
    }

    /// Видимое окно строк: материализуются ТОЛЬКО показанные строки.
    pub fn viewport(&mut self, top_line: u64, count: usize) -> io::Result<Vec<ViewportLine>> {
        if count == 0 {
            return Ok(Vec::new());
        }
        let total = self.total_len();
        let mut off = self.offset_of_line(top_line)?;
        let mut out = Vec::with_capacity(count.min(1024));
        let mut line = top_line;
        while out.len() < count && off < total {
            let hard_end = self.next_newline_from(off).map(|p| p + 1).unwrap_or(total);
            let span = (hard_end - off) as usize;
            let truncated = span > VIEWPORT_MAX_LINE_BYTES;
            let want = span.min(VIEWPORT_MAX_LINE_BYTES);
            let mut raw = self.read_range(off, want);
            if raw.last() == Some(&b'\n') {
                raw.pop();
            }
            if raw.last() == Some(&b'\r') {
                raw.pop();
            }
            out.push(ViewportLine {
                line,
                text: String::from_utf8_lossy(&raw).into_owned(),
                truncated,
            });
            line += 1;
            off = hard_end;
        }
        // Хвостовая пустая строка после финального \n (как показывают
        // редакторы: файл "a\n" — две строки, вторая пустая).
        if out.len() < count
            && total > 0
            && off >= total
            && self.byte_at(total - 1) == b'\n'
        {
            out.push(ViewportLine {
                line,
                text: String::new(),
                truncated: false,
            });
        }
        Ok(out)
    }

    // ---------------- правки ----------------

    fn snapshot_now(&self) -> Snapshot {
        Snapshot {
            pieces: self.pieces.clone(),
            piece_line_start: self.piece_line_start.clone(),
            known_prefix: self.known_prefix,
        }
    }

    fn undo_cap(&self) -> usize {
        let per = ((self.pieces.len() + 1) as u64 * 48).max(1);
        ((UNDO_SNAPSHOT_BUDGET / per) as usize).clamp(8, 512)
    }

    fn push_undo(&mut self) {
        let cap = self.undo_cap();
        self.redo.clear();
        self.undo.push(self.snapshot_now());
        while self.undo.len() > cap {
            self.undo.remove(0);
        }
    }

    /// Восстановление снапшота. Edit-буферы НЕ обрезаются: они append-only,
    /// ожившие при redo куски продолжают ссылаться на те же байты
    /// (классическая piece-table семантика «add buffer»). После restore
    /// буфер считается изменённым относительно диска (save-точка не
    /// отслеживается в v1 — консервативно, как в ранних piece-table).
    fn restore(&mut self, s: Snapshot) {
        self.pieces = s.pieces;
        self.piece_line_start = s.piece_line_start;
        self.known_prefix = s.known_prefix;
        self.rebuild_cum();
    }

    pub fn undo(&mut self) -> bool {
        if let Some(s) = self.undo.pop() {
            self.redo.push(self.snapshot_now());
            self.restore(s);
            self.dirty = true;
            true
        } else {
            false
        }
    }

    pub fn redo(&mut self) -> bool {
        if let Some(s) = self.redo.pop() {
            self.undo.push(self.snapshot_now());
            self.restore(s);
            self.dirty = true;
            true
        } else {
            false
        }
    }

    /// Сдвинуть смещение к началу UTF-8 символа (правки не рвут символы):
    /// позиция валидна, если байт В НЕЙ — не continuation (0b10xxxxxx).
    fn snap_to_char(&self, off: u64) -> u64 {
        let total = self.total_len();
        let mut o = off.min(total);
        for _ in 0..4 {
            if o == 0 || o >= total {
                break;
            }
            if self.byte_at(o) & 0xC0 != 0x80 {
                break; // o — начало символа
            }
            o -= 1;
        }
        o
    }

    /// Конец удаления расширяется ВПЕРЁД до конца затронутого символа
    /// (диапазон [s,e) никогда не оставляет разорванных символов).
    fn snap_forward_char(&self, off: u64) -> u64 {
        let total = self.total_len();
        let mut o = off.min(total);
        while o < total && self.byte_at(o) & 0xC0 == 0x80 {
            o += 1;
        }
        o
    }

    /// Разбить кусок в точке off; вернуть индекс куска, начинающегося в off.
    fn split_at(&mut self, off: u64) -> usize {
        if self.pieces.is_empty() || off == 0 {
            return 0;
        }
        let total = self.total_len();
        if off >= total {
            return self.pieces.len();
        }
        let i = self.locate(off);
        let intra = (off - self.piece_start(i)) as usize;
        if intra == 0 {
            return i;
        }
        let plen = self.pieces[i].len as usize;
        if intra == plen {
            return i + 1;
        }
        let (nl_head, nl_tail) = {
            let bytes = self.piece_bytes(i);
            let h = count_newlines(&bytes[..intra]) as u32;
            let t = self.pieces[i]
                .nl
                .map(|n| n - h)
                .unwrap_or_else(|| count_newlines(&bytes[intra..]) as u32);
            (h, t)
        };
        let (src, len) = (self.pieces[i].src, self.pieces[i].len);
        let tail = Piece {
            src: src_shifted(src, intra as u64),
            len: (len as usize - intra) as u32,
            nl: Some(nl_tail),
        };
        self.pieces[i] = Piece { src, len: intra as u32, nl: Some(nl_head) };
        self.pieces.insert(i + 1, tail);
        self.rebuild_cum();
        i + 1
    }

    /// Дописать байты в append-only буфер правок, вернуть (buf, start).
    fn append_edit(&mut self, bytes: &[u8]) -> (u32, u64) {
        if self.edit_bufs.is_empty() {
            self.edit_bufs.push(Vec::with_capacity(64 << 10));
        }
        let idx = self.edit_bufs.len() - 1;
        let start = self.edit_bufs[idx].len() as u64;
        self.edit_bufs[idx].extend_from_slice(bytes);
        (idx as u32, start)
    }

    /// Вставка текста по байтовому смещению (границы символа выравниваются).
    pub fn insert(&mut self, off: u64, text: &str) -> io::Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        let off = self.snap_to_char(off);
        self.push_undo();
        let bytes = text.as_bytes();
        let mut added: Vec<Piece> = Vec::new();
        let mut o = 0usize;
        while o < bytes.len() {
            let take = (bytes.len() - o).min(MAX_PIECE_BYTES as usize);
            let (buf, start) = self.append_edit(&bytes[o..o + take]);
            added.push(Piece {
                src: Src::Edit { buf, start },
                len: take as u32,
                nl: Some(count_newlines(&bytes[o..o + take]) as u32),
            });
            o += take;
        }
        let known_before = self.known_prefix;
        let at = self.split_at(off);
        self.pieces.splice(at..at, added);
        self.rebuild_cum();
        // Якорь — кусок ПЕРЕД точкой вставки (его line_start не тронут splice).
        self.reline_from(at.min(known_before).saturating_sub(1));
        self.dirty = true;
        Ok(())
    }

    /// Удаление диапазона [start, end) в байтах. Границы расширяются до
    /// целых UTF-8 символов: начало — назад, конец — вперёд.
    pub fn delete(&mut self, start: u64, end: u64) -> io::Result<()> {
        let total = self.total_len();
        let mut s = self.snap_to_char(start.min(total));
        let mut e = self.snap_forward_char(end.min(total));
        if s > e {
            std::mem::swap(&mut s, &mut e);
        }
        if s == e || self.pieces.is_empty() {
            return Ok(());
        }
        self.push_undo();
        let known_before = self.known_prefix;
        let a = self.split_at(s);
        let b = self.split_at(e);
        self.pieces.drain(a..b);
        self.rebuild_cum();
        // Якорь — последний уцелевший кусок ПЕРЕД вырезанным диапазоном.
        self.reline_from(a.min(known_before).saturating_sub(1));
        self.dirty = true;
        Ok(())
    }

    // ---------------- сохранение ----------------

    fn write_to(&self, dest: &Path) -> io::Result<()> {
        let mut tmp = dest.as_os_str().to_owned();
        tmp.push(".polersave");
        let tmp = PathBuf::from(tmp);
        let file = File::create(&tmp)?;
        #[cfg(unix)]
        {
            if let Ok(md) = fs::metadata(dest) {
                let _ = fs::set_permissions(&tmp, md.permissions());
            }
        }
        let mut w = BufWriter::with_capacity(8 << 20, &file);
        for i in 0..self.pieces.len() {
            w.write_all(self.piece_bytes(i))?;
        }
        w.flush()?;
        file.sync_all()?;
        drop(w);
        fs::rename(&tmp, dest)?;
        Ok(())
    }

    /// Атомарное сохранение (tmp + fsync + rename). Без правок — no-op.
    pub fn save(&mut self) -> io::Result<u64> {
        if !self.dirty {
            return Ok(self.total_len());
        }
        let len = self.total_len();
        self.write_to(&self.path.clone())?;
        self.dirty = false;
        Ok(len)
    }

    /// Сохранить в новый путь (всегда пишет).
    pub fn save_as(&mut self, path: &Path) -> io::Result<u64> {
        let len = self.total_len();
        self.write_to(path)?;
        self.path = path.to_path_buf();
        self.dirty = false;
        Ok(len)
    }

    // ---------------- поиск ----------------

    /// SIMD-поиск подстроки по всему документу прямо поверх кусков.
    /// Совпадения через границы кусков (в т.ч. правок) находятся корректно:
    /// скан каждого куска идёт с перекрытием (carry) от предыдущего.
    pub fn search(
        &mut self,
        needle: &str,
        case_sensitive: bool,
        limit: usize,
        cancel: &AtomicBool,
    ) -> io::Result<SearchStats> {
        let started = Instant::now();
        let mut out = SearchStats {
            hits: Vec::new(),
            truncated: false,
            cancelled: false,
            scanned_bytes: 0,
            elapsed: Duration::ZERO,
        };
        if needle.is_empty() || self.pieces.is_empty() {
            out.elapsed = started.elapsed();
            return Ok(out);
        }
        let mut pats: Vec<String> = Vec::new();
        if case_sensitive || needle.is_ascii() {
            pats.push(needle.to_string());
        }
        if !case_sensitive && !needle.is_ascii() {
            pats.extend(case_variants(needle));
        } else if !case_sensitive {
            // ASCII: достаточно одного паттерна + ascii_case_insensitive.
        }
        if pats.is_empty() {
            pats.push(needle.to_string());
        }
        let maxlen = pats.iter().map(|p| p.len()).max().unwrap_or(1).max(1);
        let mut builder = aho_corasick::AhoCorasick::builder();
        builder.ascii_case_insensitive(!case_sensitive && needle.is_ascii());
        let ac = builder
            .build(pats.iter().map(|p| p.as_bytes()))
            .map_err(|e| io::Error::other(format!("aho: {e}")))?;

        let mut raw: Vec<(u64, usize)> = Vec::new();
        // Перенос через границы кусков: хвост конкатенации длиной maxlen-1.
        let mut carry: Vec<u8> = Vec::with_capacity(maxlen);
        // Буфер шва: перенос + начало куска (переиспользуется, без 16МБ копий).
        let mut seam: Vec<u8> = Vec::with_capacity(2 * maxlen);
        let mut scanned: u64 = 0;
        'outer: for i in 0..self.pieces.len() {
            if cancel.load(Ordering::Relaxed) {
                out.cancelled = true;
                break;
            }
            let pstart = self.piece_start(i);
            let bytes = self.piece_bytes(i);
            let is_orig = matches!(self.pieces[i].src, Src::Orig { .. });
            if is_orig {
                if let Src::Orig { start } = self.pieces[i].src {
                    #[cfg(unix)]
                    self.advise(start, bytes.len(), libc::MADV_SEQUENTIAL);
                }
            }
            // 1) Прямой скан куска.
            for m in ac.find_iter(bytes) {
                raw.push((pstart + m.start() as u64, m.len()));
                if raw.len() >= limit {
                    out.truncated = true;
                    break 'outer;
                }
            }
            // 2) Шов: совпадения, начавшиеся в переносе и пересёкшие границу.
            if !carry.is_empty() && !bytes.is_empty() {
                let cl = carry.len();
                seam.clear();
                seam.extend_from_slice(&carry);
                seam.extend_from_slice(&bytes[..(maxlen - 1).min(bytes.len())]);
                for m in ac.find_iter(&seam) {
                    let (ms, mlen) = (m.start(), m.len());
                    if ms < cl && ms + mlen > cl {
                        // стыковое совпадение: начало в переносе, конец в куске
                        raw.push((pstart - cl as u64 + ms as u64, mlen));
                        if raw.len() >= limit {
                            out.truncated = true;
                            break 'outer;
                        }
                    }
                }
            }
            scanned += bytes.len() as u64;
            // Перенос = последние maxlen-1 байт конкатенации (кусок может
            // быть короче — тогда хвост добирается из старого переноса).
            let keep = maxlen - 1;
            if bytes.len() >= keep {
                carry.clear();
                carry.extend_from_slice(&bytes[bytes.len() - keep..]);
            } else {
                let mut nc: Vec<u8> = Vec::with_capacity(keep);
                if carry.len() + bytes.len() > keep {
                    let skip = carry.len() + bytes.len() - keep;
                    nc.extend_from_slice(&carry[skip..]);
                } else {
                    nc.extend_from_slice(&carry);
                }
                nc.extend_from_slice(bytes);
                carry = nc;
            }
            // Страницы отсканированного куска возвращаем ядру (как harvester).
            if is_orig {
                if let Src::Orig { start } = self.pieces[i].src {
                    #[cfg(unix)]
                    self.advise(start, bytes.len(), libc::MADV_DONTNEED);
                }
            }
        }
        out.scanned_bytes = scanned;

        // Прямой скан и скан шва дают hits вне порядка; курсору фазы 2
        // нужен возрастающий порядок байтов (дешёвая сортировка ≤ limit).
        raw.sort_unstable();

        // Строки/столбцы: потоковый курсор от начала документа — каждый \n
        // посещается ровно один раз на ВЕСЬ список hits (O(документа), а не
        // O(hits × кусок)). Одновременно это ленивая достройка line-index.
        let mut pos: u64 = 0;
        let mut line_start: u64 = 0;
        let mut line: u64 = 0;
        for (byte, len) in raw {
            while pos < byte {
                match self.next_newline_from(pos) {
                    Some(nl) if nl < byte => {
                        line += 1;
                        pos = nl + 1;
                        line_start = nl + 1;
                    }
                    _ => pos = byte,
                }
            }
            out.hits.push(SearchHit {
                byte,
                len,
                line,
                col: byte - line_start,
            });
        }
        out.elapsed = started.elapsed();
        Ok(out)
    }
}

/// Три варианта регистра для кириллицы/юникода (как в poler_disk_harvester).
fn case_variants(word: &str) -> Vec<String> {
    let lower = word.to_lowercase();
    let upper = word.to_uppercase();
    // Первая БУКВА (не байт!) в верхний регистр — кириллица 2-байтовая.
    let mut cap = String::new();
    let mut chars = lower.chars();
    if let Some(c) = chars.next() {
        cap.extend(c.to_uppercase());
        cap.push_str(chars.as_str());
    }
    let mut v = vec![lower, upper, cap];
    v.dedup();
    v.retain(|s| !s.is_empty());
    if v.is_empty() {
        v.push(word.to_string());
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 11
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn tmpbuf(name: &str, content: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(name);
        std::fs::write(&p, content).unwrap();
        (dir, p)
    }

    // ---------- наивная эталонная модель ----------

    fn naive_lines(text: &str) -> u64 {
        1 + text.bytes().filter(|&b| b == b'\n').count() as u64
    }

    fn naive_line_of(text: &str, off: u64) -> u64 {
        text.as_bytes()[..off as usize]
            .iter()
            .filter(|&&b| b == b'\n')
            .count() as u64
    }

    fn naive_offset_of_line(text: &str, line: u64) -> u64 {
        if line == 0 {
            return 0;
        }
        let mut n = 0u64;
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                n += 1;
                if n == line {
                    return (i + 1) as u64;
                }
            }
        }
        text.len() as u64
    }

    // ---------- базовые ----------

    #[test]
    fn open_viewport_lines() {
        let (_d, p) = tmpbuf(
            "a.txt",
            "alpha\nbeta\ngamma\n",
        );
        let mut b = PolerBuffer::open(&p).unwrap();
        assert_eq!(b.total_len(), 17);
        let vp = b.viewport(0, 10).unwrap();
        assert_eq!(vp.len(), 4); // 3 строки + хвостовая пустая
        assert_eq!(vp[0].text, "alpha");
        assert_eq!(vp[1].text, "beta");
        assert_eq!(vp[2].text, "gamma");
        assert_eq!(vp[3].text, "");
        assert_eq!(b.lines_if_known(), None); // ещё не проиндексирован
        let (lines, done) = b
            .index_all(&AtomicBool::new(false), |_, _| {})
            .unwrap();
        assert!(done);
        assert_eq!(lines, 4);
        assert_eq!(b.lines_if_known(), Some(4));
        let vp2 = b.viewport(2, 2).unwrap();
        assert_eq!(vp2.len(), 2);
        assert_eq!(vp2[0].text, "gamma");
    }

    #[test]
    fn empty_file_is_one_line() {
        let (_d, p) = tmpbuf("empty.txt", "");
        let mut b = PolerBuffer::open(&p).unwrap();
        assert_eq!(b.total_len(), 0);
        assert_eq!(b.lines_if_known(), Some(1));
        assert_eq!(b.offset_of_line(0).unwrap(), 0);
        assert_eq!(b.line_of_offset(0).unwrap(), 0);
        let vp = b.viewport(0, 5).unwrap();
        assert!(vp.is_empty());
    }

    #[test]
    fn no_trailing_newline() {
        let (_d, p) = tmpbuf("nt.txt", "one\ntwo");
        let mut b = PolerBuffer::open(&p).unwrap();
        let (lines, _) = b.index_all(&AtomicBool::new(false), |_, _| {}).unwrap();
        assert_eq!(lines, 2);
        let vp = b.viewport(1, 5).unwrap();
        assert_eq!(vp.len(), 1);
        assert_eq!(vp[0].text, "two");
    }

    // ---------- правки / undo / save ----------

    #[test]
    fn insert_delete_undo_save_roundtrip() {
        let (d, p) = tmpbuf("edit.txt", "hello world\nsecond line\n");
        let mut b = PolerBuffer::open(&p).unwrap();

        b.insert(6, "cruel ").unwrap(); // hello cruel world
        let vp = b.viewport(0, 1).unwrap();
        assert_eq!(vp[0].text, "hello cruel world");
        assert!(b.is_dirty());

        b.delete(0, 6).unwrap(); // "cruel world"
        let vp = b.viewport(0, 1).unwrap();
        assert_eq!(vp[0].text, "cruel world");

        assert!(b.undo()); // вернуть "hello "
        let vp = b.viewport(0, 1).unwrap();
        assert_eq!(vp[0].text, "hello cruel world");

        b.save().unwrap();
        let on_disk = std::fs::read_to_string(&p).unwrap();
        assert_eq!(on_disk, "hello cruel world\nsecond line\n");
        assert!(!b.is_dirty());

        assert!(b.undo()); // снять insert совсем
        b.save().unwrap();
        let on_disk2 = std::fs::read_to_string(&p).unwrap();
        assert_eq!(on_disk2, "hello world\nsecond line\n");
        assert!(!b.undo());
        drop(d);
    }

    #[test]
    fn multiline_insert_updates_lines() {
        let (_d, p) = tmpbuf("ml.txt", "a\nb\nc\n");
        let mut b = PolerBuffer::open(&p).unwrap();
        b.insert(2, "X\nY\nZ").unwrap(); // "a\nX\nY\nZb\nc\n"
        let (lines, _) = b.index_all(&AtomicBool::new(false), |_, _| {}).unwrap();
        assert_eq!(lines, 6);
        let vp = b.viewport(0, 10).unwrap();
        assert_eq!(vp[0].text, "a");
        assert_eq!(vp[1].text, "X");
        assert_eq!(vp[2].text, "Y");
        assert_eq!(vp[3].text, "Zb");
        assert_eq!(vp[4].text, "c");
        assert_eq!(vp[5].text, "");
        // redo
        assert!(b.undo());
        let (lines2, _) = b.index_all(&AtomicBool::new(false), |_, _| {}).unwrap();
        assert_eq!(lines2, 4);
        assert!(b.redo());
        let (lines3, _) = b.index_all(&AtomicBool::new(false), |_, _| {}).unwrap();
        assert_eq!(lines3, 6);
    }

    #[test]
    fn unicode_snap_no_char_split() {
        let (_d, p) = tmpbuf("uni.txt", "привіт світ\n");
        let mut b = PolerBuffer::open(&p).unwrap();
        // вставка внутрь 2-байтового символа должна сдвинуться к его началу
        b.insert(1, "X").unwrap();
        let vp = b.viewport(0, 1).unwrap();
        assert_eq!(vp[0].text, "Xпривіт світ");
        // delete с разрывом символов расширяется до целых символов:
        // raw [1,3) затрагивает 'п' и 'р' → удаляются оба
        let mut b2 = PolerBuffer::open(&p).unwrap();
        b2.delete(1, 3).unwrap();
        let vp2 = b2.viewport(0, 1).unwrap();
        assert_eq!(vp2[0].text, "ивіт світ");
    }

    // ---------- поиск ----------

    #[test]
    fn search_cross_piece_boundaries() {
        // кап 16 байт: "abcdefgh" → несколько кусков
        let text = "xxabcdefghxx abcdefgh zzabcdefgh";
        let (_d, p) = tmpbuf("cross.txt", text);
        let mut b = PolerBuffer::open_with_cap(&p, 16).unwrap();
        assert!(b.pieces_len() >= 2);
        let st = b.search("abcdefgh", true, 100, &AtomicBool::new(false)).unwrap();
        assert_eq!(st.hits.len(), 3);
        assert_eq!(st.hits[0].byte, 2);
        assert_eq!(st.hits[1].byte, 13);
        assert_eq!(st.hits[2].byte, 24);
        assert_eq!(st.hits[0].line, 0);
        assert_eq!(st.hits[0].col, 2);
    }

    #[test]
    fn search_across_edit_seam() {
        let (_d, p) = tmpbuf("seam.txt", "1234567890");
        let mut b = PolerBuffer::open_with_cap(&p, 4).unwrap();
        b.insert(5, "NEEDLE").unwrap(); // 12345NEEDLE67890
        let st = b.search("NEEDLE", true, 10, &AtomicBool::new(false)).unwrap();
        assert_eq!(st.hits.len(), 1);
        assert_eq!(st.hits[0].byte, 5);
        // совпадение, начинающееся в правке и заканчивающееся в оригинале
        let (_d2, p2) = tmpbuf("seam2.txt", "AAABBBCCC");
        let mut b2 = PolerBuffer::open_with_cap(&p2, 3).unwrap();
        b2.insert(3, "xy").unwrap(); // AAAxyBBBCCC — совпадение "xyBBB"?
        let st2 = b2.search("xyBB", true, 10, &AtomicBool::new(false)).unwrap();
        assert_eq!(st2.hits.len(), 1);
        assert_eq!(st2.hits[0].byte, 3);
    }

    #[test]
    fn search_cyrillic_case_insensitive() {
        let (_d, p) = tmpbuf("cyr.txt", "Гамільтоніан і гамільтоніан\n");
        let mut b = PolerBuffer::open(&p).unwrap();
        let st = b.search("гамільтоніан", false, 10, &AtomicBool::new(false)).unwrap();
        assert_eq!(st.hits.len(), 2);
    }

    #[test]
    fn search_limit_and_cancel() {
        let (_d, p) = tmpbuf("lim.txt", &"ab\n".repeat(50));
        let mut b = PolerBuffer::open(&p).unwrap();
        let st = b.search("ab", true, 5, &AtomicBool::new(false)).unwrap();
        assert_eq!(st.hits.len(), 5);
        assert!(st.truncated);
        let cancel = AtomicBool::new(true);
        let st2 = b.search("ab", true, 5, &cancel).unwrap();
        assert!(st2.cancelled);
    }

    // ---------- long line ----------

    #[test]
    fn long_line_truncated_in_viewport() {
        let mut giant = String::with_capacity(20_000);
        giant.push_str("start ");
        for _ in 0..19_000 {
            giant.push('x');
        }
        giant.push_str(" end\nsecond\n");
        let (_d, p) = tmpbuf("giant.txt", &giant);
        let mut b = PolerBuffer::open(&p).unwrap();
        let vp = b.viewport(0, 3).unwrap();
        assert_eq!(vp.len(), 3);
        assert!(vp[0].truncated);
        assert!(vp[0].text.len() <= VIEWPORT_MAX_LINE_BYTES);
        assert_eq!(vp[1].text, "second");
        // поиск всё равно находит хвост длинной строки
        let st = b.search(" end", true, 10, &AtomicBool::new(false)).unwrap();
        assert_eq!(st.hits.len(), 1);
    }

    // ---------- property-тест против наивной модели ----------

    fn snap_model(mb: &[u8], mut o: usize) -> usize {
        while o > 0 && o < mb.len() && mb[o] & 0xC0 == 0x80 {
            o -= 1;
        }
        o
    }

    fn snap_model_fwd(mb: &[u8], mut o: usize) -> usize {
        while o < mb.len() && mb[o] & 0xC0 == 0x80 {
            o += 1;
        }
        o
    }

    #[test]
    fn property_matches_naive_model() {
        let mut seed = String::from("line zero\nкіріллица + latin mix\n\nshort\n");
        seed.push_str(&"padding lorem ipsum dolor sit amet\n".repeat(20));
        let (_d, p) = tmpbuf("prop.txt", &seed);
        let cap = 24u64; // агрессивная нарезка: много кусков
        let mut b = PolerBuffer::open_with_cap(&p, cap).unwrap();
        let mut model = seed.clone();
        let mut rng = Rng(0xC0FFEE);

        let frags = ["αβγ", "абв\n", " hello ", "\n", "Ψ∇ε", "x", "вітаю\nсвіт\n", "0"];
        for step in 0..160 {
            let op = rng.below(10);
            let blen = b.total_len();
            if op < 5 {
                // insert: смещение выравнивается по границе символа в МОДЕЛИ,
                // буфер обязан дать тот же результат своего снапом
                let fr = frags[rng.below(frags.len() as u64) as usize];
                let off = snap_model(model.as_bytes(), rng.below(blen + 1) as usize);
                b.insert(off as u64, fr).unwrap();
                model.insert_str(off, fr);
            } else if op < 8 && blen > 0 {
                // delete: начало — назад по символам, конец — вперёд (как в буфере)
                let mut s = rng.below(blen) as usize;
                let mut e = rng.below(blen) as usize;
                if s > e {
                    std::mem::swap(&mut s, &mut e);
                }
                let mb = model.as_bytes();
                let s = snap_model(mb, s);
                let e = snap_model_fwd(mb, e.max(s));
                b.delete(s as u64, e as u64).unwrap();
                model.replace_range(s..e, "");
            } else {
                // «холостая» итерация: только сверка без правки
            }
            // сверка целостности
            assert_eq!(b.total_len(), model.len() as u64, "len @step {step}");
            let got = b.read_range(0, usize::MAX);
            assert_eq!(got, model.as_bytes(), "content @step {step}");

            // line-index сверка (ленивый префикс должен сходиться)
            let (lines, done) = b
                .index_all(&AtomicBool::new(false), |_, _| {})
                .unwrap();
            let _ = done;
            assert_eq!(Some(lines), Some(naive_lines(&model)), "lines @step {step}");

            for _k in 0..6 {
                let off = rng.below(model.len() as u64 + 1);
                let want = naive_line_of(&model, off.min(model.len() as u64));
                let got_l = b.line_of_offset(off).unwrap();
                assert_eq!(got_l, want, "line_of({off}) @step {step}");
                let want_o = naive_offset_of_line(&model, want);
                let got_o = b.offset_of_line(want).unwrap();
                assert_eq!(got_o, want_o, "offset_of({want}) @step {step}");
            }
        }
    }

    #[test]
    fn save_as_new_path() {
        let (d, p) = tmpbuf("orig.txt", "one two\n");
        let mut b = PolerBuffer::open(&p).unwrap();
        b.insert(4, "THREE ").unwrap();
        let p2 = d.path().join("saved.txt");
        b.save_as(&p2).unwrap();
        assert_eq!(
            std::fs::read_to_string(&p2).unwrap(),
            "one THREE two\n"
        );
        assert_eq!(b.path(), p2.as_path());
    }
}
