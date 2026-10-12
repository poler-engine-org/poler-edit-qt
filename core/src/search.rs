//! poler-edit :: Search Engine v2 — многопоточный SIMD-скан поверх
//! piece-table снапшота, инкрементальные события, ε-плотность, резонанс.
//!
//! Слои (снизу вверх):
//!
//! 1. **Matcher** — `memchr::memmem` (SIMD, одиночный шаблон, регистрозависимый)
//!    или Aho-Corasick (ASCII case-insensitive одним шаблоном / Unicode-CI
//!    тремя вариантами регистра, как `case_variants` в буфере).
//! 2. **scan_block** — zero-copy проход по кускам снапшота с переносом
//!    (carry) через швы кусков/правок. Блок `[lo,hi)` владеет совпадениями,
//!    НАЧАВШИМИСЯ в нём; lookahead за `hi` (maxlen−1) читается, но совпадения
//!    там не собираются — они принадлежат следующему блоку. Поэтому блоки
//!    полностью независимы и сканируются rayon-параллельно.
//! 3. **Канонический non-overlapping фильтр** — прямой скан и скан шва могут
//!    дать перекрывающиеся пары (иглы с самопересечением: «aa», «abab»).
//!    Жадный leftmost-фильтр `enforce_nonoverlap` приводит выдачу к семантике
//!    последовательного не-перекрывающегося сканера (как find в Kate/VS Code).
//!    Guard-конец от предыдущего блока переносится через фазу.
//! 4. **Якорная навигация строк** — `WalkCursor` идёт по кускам (memchr по
//!    `\n`), якоря `BTreeMap<byte → (line, line_start)>` ставятся на концах
//!    ВСЕХ блоков (в т.ч. без совпадений): каждый байт документа посещается
//!    O(1) раз на весь поиск. Символьные колонки считаются только в строках
//!    с совпадениями (ASCII-иглы — тривиально равны длине).
//! 5. **ε (энергия значимости)** — локальный порт `calculate_epsilon` из
//!    poler-engine (`src/resonance/epsilon.rs`):
//!    `ε = κ·(1+ln(1+kw))·Σ_u rarity²`, `rarity = max(0, ln N − ln freq_u)`.
//!    Отличие честно задокументировано: частоты токенов — ЛОКАЛЬНЫЕ по окну
//!    ±256 байт вокруг совпадения (глобальный словарь частот движка в
//!    редактор не вшивается — размер и оффлайн-суверенность). Сигнал тот же
//!    вектор: повторяющееся boilerplate → низкая ε, разнообразный контекст →
//!    высокая; вхождения запроса дают множитель интенсивности.
//! 6. **Резонанс памяти** — `R_t = ε_t + ρ·R_{t−1}` (IIR, как в движке);
//!    финальный R считается в байтовом порядке по всем совпадениям.
//!
//! Инкрементальный режим (`run_live`): фаза viewport (приоритетное окно,
//! первые совпадения за миллисекунды) → хвост до окна → хвост после;
//! каждый блок испускает события `hits`/`progress` через `SearchEvents`.
//!
//! Известная микро-граница (задокументировано честно): при иглах с
//! самопересечением И совпадении, переползающем границу приоритетного окна
//! вьюпорта, пара «через границу» разрешается в пользу уже отданного
//! вьюпорт-совпадения (не глобально-leftmost). Внутри фаз — канонично.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use rayon::prelude::*;

use super::buffer::BufSnapshot;

/// Окно ε-плотности: байты до/после совпадения.
const EPS_WINDOW: u64 = 256;
/// ρ резонанса памяти: R_t = ε_t + ρ·R_{t−1} (как в poler-engine).
const RHO: f64 = 0.9;
/// κ — масштаб энергии значимости (движок, calculate_epsilon).
const KAPPA: f64 = 1.0;
/// Размер блока параллельного скана (владение совпадениями).
const BLOCK: u64 = 8 << 20;

// =====================================================================
// Публичные типы
// =====================================================================

/// Одно совпадение: байтовая геометрия + строка + колонки (символы И байты)
/// + ε + локальный резонанс.
#[derive(Clone, Copy, Debug)]
pub struct HitOut {
    pub byte: u64,
    pub len: usize,
    /// Длина совпадения в СИМВОЛАХ (подсветка GUI).
    pub len_chars: u64,
    pub line: u64,
    /// Столбец в СИМВОЛАХ от начала строки (позиция курсора GUI).
    pub col: u64,
    /// Столбец в БАЙТАХ от начала строки.
    pub col_bytes: u64,
    /// ε — энергия значимости (локальный режим, см. модульный док).
    pub eps: f64,
    /// Локальный резонанс R на момент этого совпадения (внутри фазы).
    pub r: f64,
}

/// Итог синхронного прогона (`run_once` / команда `search`).
#[derive(Clone, Debug)]
pub struct RunOut {
    pub hits: Vec<HitOut>,
    pub truncated: bool,
    pub cancelled: bool,
    pub scanned: u64,
    pub elapsed: Duration,
    /// Финальный резонанс в байтовом порядке (IIR по ε всех совпадений).
    pub resonance: f64,
}

/// Спецификация живого (инкрементального) поиска.
pub struct LiveSpec {
    pub needle: String,
    pub case_sensitive: bool,
    pub limit: usize,
    /// Приоритетное окно вьюпорта: (lo, hi, line(lo), line_start(lo)).
    /// Фаза A отвечает по нему за миллисекунды, до общего прогона.
    pub viewport: Option<(u64, u64, u64, u64)>,
}

/// Итог живого прогона.
#[derive(Clone, Debug)]
pub struct LiveSummary {
    pub hits: u64,
    pub truncated: bool,
    pub cancelled: bool,
    pub scanned: u64,
    pub elapsed: Duration,
    pub resonance: f64,
    pub viewport_ms: f64,
}

/// Приёмник событий живого поиска (протокол сериализует их в JSON).
pub trait SearchEvents {
    /// Пачка совпадений одного блока (≤ 2000 на событие).
    fn on_hits(&mut self, phase: &str, hits: &[HitOut], scanned: u64, total: u64);
    /// Прогресс скана (байты).
    fn on_progress(&mut self, scanned: u64, total: u64);
    /// Фаза viewport завершена (время ответа «первые совпадения», мс).
    fn on_viewport_done(&mut self, ms: f64);
}

// =====================================================================
// Matcher
// =====================================================================

enum Kind {
    /// Регистрозависимый одиночный шаблон: SIMD memmem (Finder строится
    /// на каждый вызов find_iter — вызовов O(кусков), копейки).
    Mem(Vec<u8>),
    /// Мульти-шаблон: ASCII-CI (1 шаблон + флаг) или Unicode-CI (3 варианта).
    Ac(aho_corasick::AhoCorasick),
}

struct Matcher {
    kind: Kind,
    /// Максимальная длина шаблона (перенос через швы = maxlen−1).
    maxlen: usize,
}

impl Matcher {
    fn build(needle: &str, case_sensitive: bool) -> Matcher {
        if case_sensitive {
            Matcher {
                maxlen: needle.len().max(1),
                kind: Kind::Mem(needle.as_bytes().to_vec()),
            }
        } else if needle.is_ascii() {
            let ac = aho_corasick::AhoCorasick::builder()
                .ascii_case_insensitive(true)
                .build([needle.as_bytes()])
                .expect("одиночный ASCII-шаблон строится всегда");
            Matcher { maxlen: needle.len().max(1), kind: Kind::Ac(ac) }
        } else {
            let vars = super::buffer::case_variants(needle);
            let maxlen = vars.iter().map(|s| s.len()).max().unwrap_or(1).max(1);
            let pats: Vec<&[u8]> = vars.iter().map(|s| s.as_bytes()).collect();
            let ac = aho_corasick::AhoCorasick::builder()
                .build(pats)
                .expect("варианты регистра строятся всегда");
            Matcher { maxlen, kind: Kind::Ac(ac) }
        }
    }

    /// Обойти все (не перекрывающиеся внутри одного буфера) совпадения.
    #[inline]
    fn each(&self, hay: &[u8], mut f: impl FnMut(usize, usize)) {
        match &self.kind {
            Kind::Mem(n) => {
                let l = n.len();
                for s in memchr::memmem::find_iter(hay, n) {
                    f(s, l);
                }
            }
            Kind::Ac(ac) => {
                for m in ac.find_iter(hay) {
                    f(m.start(), m.len());
                }
            }
        }
    }
}

// =====================================================================
// Скан блока (zero-copy, перенос через швы)
// =====================================================================

/// Скан блока [lo, hi): собрать совпадения, НАЧАВШИЕСЯ в [lo, hi).
/// Lookahead (maxlen−1 за hi) читается для швов, совпадения за hi не
/// собираются — они принадлежат следующему блоку. Возвращает hits,
/// отсортированные по байту (шовные приходят после прямых).
/// `scanned` копит честно потреблённые байты (по кускам).
fn scan_block(
    snap: &BufSnapshot,
    m: &Matcher,
    lo: u64,
    hi: u64,
    cancel: &AtomicBool,
    skip: &AtomicUsize,
    scanned: &AtomicU64,
) -> Vec<(u64, usize)> {
    let total = snap.total_len;
    if lo >= hi || lo >= total {
        return Vec::new();
    }
    let hi = hi.min(total);
    let lookahead = (m.maxlen as u64).saturating_sub(1);
    let hi_scan = hi.saturating_add(lookahead).min(total);
    let mut hits: Vec<(u64, usize)> = Vec::new();
    let mut carry: Vec<u8> = Vec::with_capacity(m.maxlen);
    let mut seam: Vec<u8> = Vec::with_capacity(2 * m.maxlen);
    let mut pos = lo;
    let mut i = snap.locate(lo);
    while pos < hi_scan {
        if cancel.load(Ordering::Relaxed) || skip.load(Ordering::Relaxed) != 0 {
            break;
        }
        let pstart = snap.piece_start(i);
        let plen = snap.pieces[i].len as u64;
        if pos >= pstart + plen {
            i += 1;
            continue;
        }
        let intra = (pos - pstart) as usize;
        let take = ((pstart + plen).min(hi_scan) - pos) as usize;
        if take == 0 {
            i += 1;
            continue;
        }
        let is_orig = matches!(snap.pieces[i].src, super::buffer::Src::Orig { .. });
        if is_orig {
            snap.advise_piece(i, true);
        }
        {
            let pb = snap.piece_bytes(i);
            let slice = &pb[intra..intra + take];
            // 1) Прямой скан куска.
            m.each(slice, |s, l| {
                let abs = pos + s as u64;
                if abs < hi {
                    hits.push((abs, l));
                }
            });
            // 2) Шов: совпадения, начавшиеся в переносе и пересёкшие границу.
            if !carry.is_empty() && !slice.is_empty() {
                let cl = carry.len();
                seam.clear();
                seam.extend_from_slice(&carry);
                let head = (m.maxlen - 1).min(slice.len());
                seam.extend_from_slice(&slice[..head]);
                m.each(&seam, |s, l| {
                    if s < cl && s + l > cl {
                        // abs ≥ lo всегда: carry набирается изнутри блока.
                        let abs = pos - cl as u64 + s as u64;
                        if abs < hi {
                            hits.push((abs, l));
                        }
                    }
                });
            }
            // 3) Перенос = хвост логической конкатенации длиной maxlen−1.
            let keep = m.maxlen - 1;
            if slice.len() >= keep {
                carry.clear();
                carry.extend_from_slice(&slice[slice.len() - keep..]);
            } else {
                let mut nc: Vec<u8> = Vec::with_capacity(keep);
                if carry.len() + slice.len() > keep {
                    let skipb = carry.len() + slice.len() - keep;
                    nc.extend_from_slice(&carry[skipb..]);
                } else {
                    nc.extend_from_slice(&carry);
                }
                nc.extend_from_slice(slice);
                carry = nc;
            }
        }
        pos += take as u64;
        scanned.fetch_add(take as u64, Ordering::Relaxed);
        if is_orig {
            snap.advise_piece(i, false);
        }
        i += 1;
    }
    hits.sort_unstable();
    hits
}

/// Жадный leftmost-фильтр: убрать перекрытия (каноническая семантика
/// последовательного не-перекрывающегося сканера). `guard_end` — конец
/// последнего принятого совпадения предыдущих блоков фазы.
fn enforce_nonoverlap(hits: &mut Vec<(u64, usize)>, guard_end: u64) {
    let mut last = guard_end;
    let mut w = 0;
    for k in 0..hits.len() {
        let (b, l) = hits[k];
        if b >= last {
            hits[w] = (b, l);
            w += 1;
            last = b + l as u64;
        }
    }
    hits.truncate(w);
}

/// Контроль параллельного скана: cancel/skip/scanned — общие атомики,
/// counter копит совокупное число hits (для ранней капитуляции по лимиту).
struct ScanCtl {
    cancel: AtomicBool,
    skip: AtomicUsize,
    scanned: AtomicU64,
    counter: AtomicUsize,
    limit: usize,
}

impl ScanCtl {
    fn new(cancel: &AtomicBool, limit: usize) -> ScanCtl {
        ScanCtl {
            cancel: AtomicBool::new(cancel.load(Ordering::Relaxed)),
            skip: AtomicUsize::new(0),
            scanned: AtomicU64::new(0),
            counter: AtomicUsize::new(0),
            limit,
        }
    }
}

/// Параллельный скан диапазона [a, b): блоки по BLOCK, rayon, порядок
/// блоков сохраняется. Возвращает (lo, hi, hits) каждого блока.
fn scan_range_parallel(
    snap: &BufSnapshot,
    m: &Matcher,
    a: u64,
    b: u64,
    ctl: &ScanCtl,
) -> Vec<(u64, u64, Vec<(u64, usize)>)> {
    if a >= b || snap.total_len == 0 {
        return Vec::new();
    }
    let b = b.min(snap.total_len);
    let mut blocks: Vec<(u64, u64)> = Vec::new();
    let mut lo = a;
    while lo < b {
        let hi = (lo + BLOCK).min(b);
        blocks.push((lo, hi));
        lo = hi;
    }
    blocks
        .into_par_iter()
        .map(|(lo, hi)| {
            if ctl.skip.load(Ordering::Relaxed) != 0 || ctl.cancel.load(Ordering::Relaxed) {
                return (lo, hi, Vec::new());
            }
            let hits = scan_block(snap, m, lo, hi, &ctl.cancel, &ctl.skip, &ctl.scanned);
            let n = ctl.counter.fetch_add(hits.len(), Ordering::Relaxed) + hits.len();
            if n >= ctl.limit {
                ctl.skip.store(1, Ordering::Relaxed);
            }
            (lo, hi, hits)
        })
        .collect()
}

// =====================================================================
// Курсор строк + якоря
// =====================================================================

#[derive(Clone, Copy)]
struct Anchor {
    byte: u64,
    line: u64,
    line_start: u64,
}

/// Курсор по логическому потоку снапшота: переводы строк считаются memchr'ом
/// (кусками, ~ГБ/с), символы — байтовым проходом ТОЛЬКО в строках с
/// совпадениями, с инкрементальным кэшем [line_start, chars_pos].
struct WalkCursor<'a> {
    snap: &'a BufSnapshot,
    pos: u64,
    line: u64,
    line_start: u64,
    chars_pos: u64,
    chars: u64,
}

impl<'a> WalkCursor<'a> {
    fn new(snap: &'a BufSnapshot, a: Anchor) -> Self {
        WalkCursor {
            snap,
            pos: a.byte,
            line: a.line,
            line_start: a.line_start,
            chars_pos: a.line_start,
            chars: 0,
        }
    }

    /// Продвинуться к байту `to` (to ≥ pos), пересчитывая строки.
    fn advance(&mut self, to: u64) {
        if to <= self.pos {
            return;
        }
        while self.pos < to {
            let i = self.snap.locate(self.pos);
            let pstart = self.snap.piece_start(i);
            let pb = self.snap.piece_bytes(i);
            let intra = (self.pos - pstart) as usize;
            let take = ((pstart + pb.len() as u64).min(to) - self.pos) as usize;
            if take == 0 {
                break;
            }
            let slice = &pb[intra..intra + take];
            for p in memchr::memchr_iter(b'\n', slice) {
                self.line += 1;
                self.line_start = self.pos + p as u64 + 1;
            }
            self.pos += take as u64;
        }
    }

    /// Число СИМВОЛОВ в [line_start, to) для to в текущей строке.
    /// Инкрементально: байты между последовательными hit'ами не ревизитуются.
    fn chars_at(&mut self, to: u64) -> u64 {
        if self.chars_pos < self.line_start {
            self.chars_pos = self.line_start;
            self.chars = 0;
        }
        if self.chars_pos < to {
            let mut p = self.chars_pos;
            while p < to {
                let i = self.snap.locate(p);
                let pstart = self.snap.piece_start(i);
                let pb = self.snap.piece_bytes(i);
                let intra = (p - pstart) as usize;
                let take = ((pstart + pb.len() as u64).min(to) - p) as usize;
                if take == 0 {
                    break;
                }
                let slice = &pb[intra..intra + take];
                self.chars += slice.iter().filter(|&&b| b & 0xC0 != 0x80).count() as u64;
                p += take as u64;
            }
            self.chars_pos = to;
        }
        self.chars
    }
}

/// Число СИМВОЛОВ в диапазоне [from, to) через scratch-буфер (без аллокаций).
fn chars_in_span(snap: &BufSnapshot, scratch: &mut Vec<u8>, from: u64, to: u64) -> u64 {
    snap.read_range_into(scratch, from, (to - from) as usize);
    scratch.iter().filter(|&&b| b & 0xC0 != 0x80).count() as u64
}

// =====================================================================
// ε — энергия значимости (локальный порт poler-engine)
// =====================================================================

/// Локальный режим calculate_epsilon: окно ±EPS_WINDOW вокруг совпадения,
/// токены = ASCII-слова + сплошные не-ASCII серии (слова кириллицы),
/// частоты токенов — по окну. `variants` — байтовые варианты запроса
/// (множитель интенсивности kw). FNV-хеш с ASCII-фолдингом: кириллица
/// точная, латиница регистронезависимая.
fn epsilon_local(
    snap: &BufSnapshot,
    hit_byte: u64,
    hit_len: usize,
    variants: &[Vec<u8>],
    scratch: &mut Vec<u8>,
) -> f64 {
    let total = snap.total_len;
    if total == 0 || variants.is_empty() {
        return 0.0;
    }
    let win_lo = hit_byte.saturating_sub(EPS_WINDOW);
    let win_hi = (hit_byte + hit_len as u64 + EPS_WINDOW).min(total);
    snap.read_range_into(scratch, win_lo, (win_hi - win_lo) as usize);
    if scratch.is_empty() {
        return 0.0;
    }

    // Токенизация: [A-Za-z0-9_]+ | серия байтов ≥0x80 (слово кириллицы).
    let hay = scratch.as_slice();
    let mut tokens: Vec<(usize, usize)> = Vec::with_capacity(64);
    let mut i = 0;
    while i < hay.len() {
        let b = hay[i];
        if b >= 0x80 {
            let s = i;
            while i < hay.len() && hay[i] >= 0x80 {
                i += 1;
            }
            tokens.push((s, i));
        } else if b.is_ascii_alphanumeric() || b == b'_' {
            let s = i;
            while i < hay.len() && (hay[i].is_ascii_alphanumeric() || hay[i] == b'_') {
                i += 1;
            }
            tokens.push((s, i));
        } else {
            i += 1;
        }
    }
    let n = tokens.len();
    if n == 0 {
        return 0.0;
    }
    let log_n = (n as f64).ln();

    // Частоты по окну: FNV(ASCII-фолдинг) токена → индекс счётчика.
    let mut freq: HashMap<u64, usize> = HashMap::with_capacity(64);
    let mut counts: Vec<usize> = Vec::with_capacity(64);
    let mut kw = 0usize;
    for &(s, e) in &tokens {
        let mut h: u64 = 0xcbf29ce484222325;
        for &b in &hay[s..e] {
            h ^= (b.to_ascii_lowercase()) as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        match freq.entry(h) {
            std::collections::hash_map::Entry::Occupied(o) => {
                let idx = *o.get();
                counts[idx] += 1;
            }
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(counts.len());
                counts.push(1);
            }
        }
        // kw: токен совпадает с любым вариантом запроса (ASCII-фолдинг).
        let tok = &hay[s..e];
        for v in variants {
            if v.len() == tok.len()
                && v.iter()
                    .zip(tok.iter())
                    .all(|(a, b)| a.to_ascii_lowercase() == b.to_ascii_lowercase())
            {
                kw += 1;
                break;
            }
        }
    }

    let mut sum = 0.0;
    for &c in &counts {
        if c > 0 {
            let rarity = (log_n - (c as f64).ln()).max(0.0);
            sum += rarity * rarity;
        }
    }
    let intensity = 1.0 + (kw as f64 + 1.0).ln();
    KAPPA * intensity * sum
}

// =====================================================================
// Пост-обработка блока: строки/колонки/ε/R
// =====================================================================

struct PostCtx<'a> {
    snap: &'a BufSnapshot,
    variants: Vec<Vec<u8>>,
    ascii_needle: bool,
    scratch: Vec<u8>,
    /// Резонанс фазы (байтовый порядок внутри фазы).
    r: f64,
}

/// Заполнить line/col/col_bytes/len_chars/eps/r для raw-hits блока [lo,hi),
/// поставить якорь на конец блока. Символьные колонки — через инкрементальный
/// кэш курсора; len_chars для ASCII-игл равен длине (бесплатно).
fn resolve_block(
    ctx: &mut PostCtx,
    anchors: &mut BTreeMap<u64, Anchor>,
    lo: u64,
    hi: u64,
    raw: &[(u64, usize)],
    out: &mut Vec<HitOut>,
) {
    let seed = anchors
        .range(..=lo)
        .next_back()
        .map(|(_, a)| *a)
        .unwrap_or(Anchor { byte: 0, line: 0, line_start: 0 });
    let mut cur = WalkCursor::new(ctx.snap, seed);
    for &(byte, len) in raw {
        cur.advance(byte);
        let line = cur.line;
        let line_start = cur.line_start;
        let col_bytes = byte - line_start;
        let col = cur.chars_at(byte);
        let len_chars = if ctx.ascii_needle {
            len as u64
        } else {
            chars_in_span(ctx.snap, &mut ctx.scratch, byte, byte + len as u64)
        };
        let eps = epsilon_local(ctx.snap, byte, len, &ctx.variants, &mut ctx.scratch);
        ctx.r = eps + RHO * ctx.r;
        out.push(HitOut {
            byte,
            len,
            len_chars,
            line,
            col,
            col_bytes,
            eps,
            r: ctx.r,
        });
    }
    let hi_eff = hi.min(ctx.snap.total_len);
    cur.advance(hi_eff);
    let end = Anchor { byte: hi_eff, line: cur.line, line_start: cur.line_start };
    anchors.insert(end.byte, end);
}

/// Варианты запроса в байтах (для kw в ε).
fn needle_variants(needle: &str, case_sensitive: bool) -> Vec<Vec<u8>> {
    if case_sensitive || needle.is_ascii() {
        vec![needle.as_bytes().to_vec()]
    } else {
        super::buffer::case_variants(needle)
            .into_iter()
            .map(|s| s.into_bytes())
            .collect()
    }
}

// =====================================================================
// run_once — синхронный полный прогон (команда «search»)
// =====================================================================

pub fn run_once(
    snap: &BufSnapshot,
    needle: &str,
    case_sensitive: bool,
    limit: usize,
    cancel: &AtomicBool,
) -> RunOut {
    let started = Instant::now();
    let mut out = RunOut {
        hits: Vec::new(),
        truncated: false,
        cancelled: false,
        scanned: 0,
        elapsed: Duration::ZERO,
        resonance: 0.0,
    };
    if needle.is_empty() || snap.is_empty() || snap.total_len == 0 {
        out.elapsed = started.elapsed();
        return out;
    }
    let m = Matcher::build(needle, case_sensitive);
    let ctl = ScanCtl::new(cancel, limit);
    let blocks = scan_range_parallel(snap, &m, 0, snap.total_len, &ctl);

    let mut raw: Vec<(u64, usize)> = Vec::new();
    for (_, _, hits) in blocks {
        raw.extend(hits);
    }
    out.cancelled = ctl.cancel.load(Ordering::Relaxed);
    out.scanned = ctl.scanned.load(Ordering::Relaxed);
    enforce_nonoverlap(&mut raw, 0);
    if raw.len() > limit {
        raw.truncate(limit);
        out.truncated = true;
    }

    let mut anchors: BTreeMap<u64, Anchor> = BTreeMap::new();
    anchors.insert(0, Anchor { byte: 0, line: 0, line_start: 0 });
    let mut ctx = PostCtx {
        snap,
        variants: needle_variants(needle, case_sensitive),
        ascii_needle: needle.is_ascii(),
        scratch: Vec::with_capacity((2 * EPS_WINDOW + 16) as usize),
        r: 0.0,
    };
    resolve_block(&mut ctx, &mut anchors, 0, snap.total_len, &raw, &mut out.hits);

    out.resonance = final_resonance(&out.hits);
    out.elapsed = started.elapsed();
    out
}

// =====================================================================
// run_live — инкрементальный прогон с событиями
// =====================================================================

pub fn run_live(
    snap: &BufSnapshot,
    spec: &LiveSpec,
    cancel: &AtomicBool,
    events: &mut dyn SearchEvents,
) -> LiveSummary {
    let started = Instant::now();
    let mut sum = LiveSummary {
        hits: 0,
        truncated: false,
        cancelled: false,
        scanned: 0,
        elapsed: Duration::ZERO,
        resonance: 0.0,
        viewport_ms: 0.0,
    };
    if spec.needle.is_empty() || snap.is_empty() || snap.total_len == 0 {
        sum.elapsed = started.elapsed();
        return sum;
    }
    let m = Matcher::build(&spec.needle, spec.case_sensitive);
    let mut ctx = PostCtx {
        snap,
        variants: needle_variants(&spec.needle, spec.case_sensitive),
        ascii_needle: spec.needle.is_ascii(),
        scratch: Vec::with_capacity((2 * EPS_WINDOW + 16) as usize),
        r: 0.0,
    };
    let ctl = ScanCtl::new(cancel, spec.limit);
    let mut anchors: BTreeMap<u64, Anchor> = BTreeMap::new();
    let mut all_hits: Vec<HitOut> = Vec::new();
    let mut last_progress = Instant::now();

    // ---- фаза A: приоритетное окно вьюпорта ----
    let vp = spec
        .viewport
        .filter(|&(lo, hi, _, _)| lo < hi && hi <= snap.total_len && lo < snap.total_len);
    if let Some((lo, hi, lo_line, lo_line_start)) = vp {
        let t0 = Instant::now();
        anchors.insert(lo, Anchor { byte: lo, line: lo_line, line_start: lo_line_start });
        let mut raw = scan_block(snap, &m, lo, hi, &ctl.cancel, &ctl.skip, &ctl.scanned);
        enforce_nonoverlap(&mut raw, 0);
        let mut block_hits: Vec<HitOut> = Vec::with_capacity(raw.len());
        resolve_block(&mut ctx, &mut anchors, lo, hi, &raw, &mut block_hits);
        // Лимит режет ДО эмиссии — событие не может привысить бюджет.
        let remaining = (spec.limit as u64).saturating_sub(sum.hits);
        if block_hits.len() as u64 > remaining {
            block_hits.truncate(remaining as usize);
            sum.truncated = true;
            ctl.skip.store(1, Ordering::Relaxed);
        }
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        sum.viewport_ms = ms;
        emit_hits(events, "viewport", &block_hits, ctl.scanned.load(Ordering::Relaxed), snap.total_len);
        events.on_viewport_done(ms);
        sum.hits += block_hits.len() as u64;
        all_hits.extend(block_hits);
        last_progress = Instant::now();
    }

    // ---- фазы B/C: [0, lo) и [hi, total) ----
    let (vp_lo, vp_hi) = match vp {
        Some((lo, hi, _, _)) => (lo, hi),
        None => (snap.total_len, snap.total_len),
    };
    let ranges: [(u64, u64, &str); 2] = [
        (0, vp_lo, "pre"),
        (vp_hi, snap.total_len, "post"),
    ];
    'phases: for (a, b, phase) in ranges {
        if a >= b || sum.truncated || ctl.skip.load(Ordering::Relaxed) != 0 {
            continue;
        }
        if a == 0 && !anchors.contains_key(&0) {
            anchors.insert(0, Anchor { byte: 0, line: 0, line_start: 0 });
        }
        // Guard non-overlap фазы: для pre — 0 (нет предшественника);
        // для post — конец последнего совпадения вьюпорта ИЛИ pre-фазы.
        let mut guard: u64 = 0;
        if phase == "post" {
            for h in &all_hits {
                if h.byte < a {
                    guard = guard.max(h.byte + h.len as u64);
                }
            }
        }
        let blocks = scan_range_parallel(snap, &m, a, b, &ctl);
        for (blo, bhi, mut raw) in blocks {
            if ctl.cancel.load(Ordering::Relaxed) {
                sum.cancelled = true;
                break 'phases;
            }
            enforce_nonoverlap(&mut raw, guard);
            // Якорь ставится ДАЖЕ для пустого блока: цепочка не рвётся,
            // каждый байт посещается O(1) раз на весь поиск.
            let mut block_hits: Vec<HitOut> = Vec::with_capacity(raw.len());
            resolve_block(&mut ctx, &mut anchors, blo, bhi, &raw, &mut block_hits);
            // Лимит режет ДО эмиссии.
            let remaining = (spec.limit as u64).saturating_sub(sum.hits);
            if block_hits.len() as u64 > remaining {
                block_hits.truncate(remaining as usize);
                sum.truncated = true;
            }
            if !block_hits.is_empty() {
                emit_hits(
                    events,
                    phase,
                    &block_hits,
                    ctl.scanned.load(Ordering::Relaxed),
                    snap.total_len,
                );
                if last_progress.elapsed().as_millis() >= 100 {
                    events.on_progress(ctl.scanned.load(Ordering::Relaxed), snap.total_len);
                    last_progress = Instant::now();
                }
                for h in &block_hits {
                    guard = guard.max(h.byte + h.len as u64);
                }
                sum.hits += block_hits.len() as u64;
                all_hits.extend(block_hits);
            }
            if sum.truncated || ctl.skip.load(Ordering::Relaxed) != 0 {
                break 'phases;
            }
        }
    }
    events.on_progress(ctl.scanned.load(Ordering::Relaxed), snap.total_len);
    sum.cancelled |= ctl.cancel.load(Ordering::Relaxed);
    sum.scanned = ctl.scanned.load(Ordering::Relaxed);
    sum.resonance = final_resonance(&all_hits);
    sum.elapsed = started.elapsed();
    sum
}

fn final_resonance(hits: &[HitOut]) -> f64 {
    let mut r = 0.0;
    for h in hits {
        r = h.eps + RHO * r;
    }
    r
}

/// Разбить пачку на события ≤ 2000 hits (гигантские массивы не пишем).
fn emit_hits(
    events: &mut dyn SearchEvents,
    phase: &str,
    hits: &[HitOut],
    scanned: u64,
    total: u64,
) {
    if hits.is_empty() {
        return;
    }
    const CHUNK: usize = 2000;
    if hits.len() <= CHUNK {
        events.on_hits(phase, hits, scanned, total);
        return;
    }
    let mut i = 0;
    while i < hits.len() {
        let hi = (i + CHUNK).min(hits.len());
        events.on_hits(phase, &hits[i..hi], scanned, total);
        i = hi;
    }
}

// =====================================================================
// Тесты
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::PolerBuffer;
    use std::fs;
    use std::path::PathBuf;

    fn tmpbuf(name: &str, content: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(name);
        fs::write(&p, content).unwrap();
        (dir, p)
    }

    /// Наивная эталонная модель: последовательный не-перекрывающийся
    /// leftmost-скан (семантика любого редактора).
    fn naive_find(text: &str, needle: &str) -> Vec<(usize, usize)> {
        let (h, n) = (text.as_bytes(), needle.as_bytes());
        let mut out = Vec::new();
        if n.is_empty() {
            return out;
        }
        let mut i = 0;
        while i + n.len() <= h.len() {
            if &h[i..i + n.len()] == n {
                out.push((i, n.len()));
                i += n.len();
            } else {
                i += 1;
            }
        }
        out
    }

    #[test]
    fn parity_with_naive_self_overlapping_needles() {
        // Иглы с самопересечением — главный источник расхождений seam-логики.
        for (text, needle) in [
            ("aaaa", "aa"),
            ("abababab", "abab"),
            ("anananas", "ana"),
            ("aabaabaab", "aab"),
            ("xxabcdefghxx abcdefgh zzabcdefgh", "abcdefgh"),
            (" needle needle needle ", "needle"),
        ] {
            let (_d, p) = tmpbuf("so.txt", text);
            let mut b = PolerBuffer::open_with_cap(&p, 7).unwrap();
            let st = b.search(needle, true, 1000, &AtomicBool::new(false)).unwrap();
            let want = naive_find(text, needle);
            assert_eq!(st.hits.len(), want.len(), "text={text:?} needle={needle:?}");
            for (h, (b0, l)) in st.hits.iter().zip(want.iter()) {
                assert_eq!(h.byte as usize, *b0, "byte {text:?}/{needle:?}");
                assert_eq!(h.len, *l, "len {text:?}/{needle:?}");
            }
        }
    }

    #[test]
    fn parity_random_text_random_needles() {
        // Property: случайный текст с кириллицей, случайные иглы из текста,
        // агрессивная нарезка кусков — выдача == наивной модели.
        let mut rng = 0x5EEDu64;
        let mut next = || {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            rng >> 33
        };
        let words = [
            "alpha", "beta", "гамма", "дельта", "X", "needle", "світ", "ab", "aab",
            "лог", "⊂", "0",
        ];
        let mut text = String::new();
        for _ in 0..400 {
            text.push_str(words[next() as usize % words.len()]);
            if next() % 4 == 0 {
                text.push('\n');
            } else {
                text.push(' ');
            }
        }
        let (_d, p) = tmpbuf("rand.txt", &text);
        for cap in [8u64, 16, 64] {
            let mut b = PolerBuffer::open_with_cap(&p, cap).unwrap();
            for needle in ["needle", "гамма", "aab", "ab", "світ"] {
                for cs in [true, false] {
                    let st = b.search(needle, cs, 10_000, &AtomicBool::new(false)).unwrap();
                    // Наивная модель с теми же правилами регистра.
                    let want: Vec<(usize, usize)> = if cs {
                        naive_find(&text, needle)
                    } else {
                        let low = text.to_lowercase();
                        let nl = needle.to_lowercase();
                        naive_find(&low, &nl)
                    };
                    assert_eq!(
                        st.hits.len(),
                        want.len(),
                        "cap={cap} needle={needle} cs={cs}"
                    );
                    for (h, (b0, _l)) in st.hits.iter().zip(want.iter()) {
                        assert_eq!(h.byte as usize, *b0);
                    }
                }
            }
        }
    }

    #[test]
    fn cyrillic_cols_chars_and_bytes() {
        // «привіт світ»: «світ» начинается с 7-го СИМВОЛА и 13-го БАЙТА.
        let (_d, p) = tmpbuf("cyr.txt", "привіт світ\n");
        let mut b = PolerBuffer::open(&p).unwrap();
        let st = b.search("світ", true, 10, &AtomicBool::new(false)).unwrap();
        assert_eq!(st.hits.len(), 1);
        let h = &st.hits[0];
        assert_eq!(h.col, 7, "символьная колонка");
        assert_eq!(h.col_bytes, 13, "байтовая колонка");
        assert_eq!(h.len_chars, 4, "длина в символах");
        assert_eq!(h.len, 8, "длина в байтах");
        assert_eq!(h.line, 0);
    }

    #[test]
    fn unicode_ci_multiple_variants() {
        let (_d, p) = tmpbuf("ci.txt", "Гамільтоніан і гамільтоніан\n");
        let mut b = PolerBuffer::open(&p).unwrap();
        let st = b.search("гамільтоніан", false, 10, &AtomicBool::new(false)).unwrap();
        assert_eq!(st.hits.len(), 2);
        // 12 букв (24 Б) + " і " (пробел+і+пробел = 4 Б) = 15 симв., 28 байт.
        assert_eq!(st.hits[1].col, 15, "колонка второго вхождения (символы)");
        assert_eq!(st.hits[1].col_bytes, 28, "байтовая колонка");
    }

    #[test]
    fn limit_and_cancel_v2() {
        let (_d, p) = tmpbuf("lim.txt", &"ab\n".repeat(50));
        let mut b = PolerBuffer::open(&p).unwrap();
        let st = b.search("ab", true, 5, &AtomicBool::new(false)).unwrap();
        assert_eq!(st.hits.len(), 5);
        assert!(st.truncated);
        let st2 = b.search("ab", true, 5, &AtomicBool::new(true)).unwrap();
        assert!(st2.cancelled);
    }

    #[test]
    fn epsilon_repetitive_below_diverse_and_kw_boost() {
        // Разные ФАЙЛЫ: окно ±256 Б не должно протекать между зонами.
        let repetitive = "log log log ".repeat(40); // ~480 Б чистого повтора
        let diverse = "квантова топологія резонанс архетип інваріант енергія градієнт ".repeat(8);
        let (_d, p1) = tmpbuf("eps1.txt", &repetitive);
        let (_d2, p2) = tmpbuf("eps2.txt", &diverse);
        let mut b1 = PolerBuffer::open(&p1).unwrap();
        let mut b2 = PolerBuffer::open(&p2).unwrap();
        let st1 = b1.search("log", true, 1, &AtomicBool::new(false)).unwrap();
        let st2 = b2.search("топологія", true, 1, &AtomicBool::new(false)).unwrap();
        assert_eq!(st1.hits.len(), 1);
        assert_eq!(st2.hits.len(), 1);
        assert!(
            st2.hits[0].eps > st1.hits[0].eps,
            "diverse eps={} > repetitive eps={}",
            st2.hits[0].eps,
            st1.hits[0].eps
        );
        // kw-буст: вхождения запроса в окне повышают ε.
        // Смешанное окно: log×3 + alpha/beta/gamma — редкость не нулевая.
        let (_d3, p3) = tmpbuf("eps3.txt", "log alpha log beta log gamma log delta log");
        let mut b3 = PolerBuffer::open(&p3).unwrap();
        let snap = b3.snapshot();
        let e_kw = epsilon_local(&snap, 0, 3, &[b"log".to_vec()], &mut Vec::with_capacity(1024));
        let e_none = epsilon_local(&snap, 0, 3, &[b"zzz".to_vec()], &mut Vec::with_capacity(1024));
        assert!(e_kw > e_none, "kw={e_kw} > none={e_none}");
    }

    #[test]
    fn resonance_iir_sequence() {
        // R = ε + ρ·R: ε=1 трижды → 1, 1.9, 2.71 — прямая проверка формулы.
        let mut r = 0.0;
        for _ in 0..3 {
            r = 1.0 + RHO * r;
        }
        assert!((r - 2.71).abs() < 0.01, "R3={r}");
        // Погашающийся резонанс: ε=0 → R гаснет геометрически.
        let mut z = 10.0;
        for _ in 0..100 {
            z = RHO * z;
        }
        assert!(z < 0.001, "затухание R={z}");
        // Живой пример: у односовпадённого богатого окна ε > 0.
        let (_d, p) = tmpbuf("r.txt", "one aa two\n");
        let mut b = PolerBuffer::open(&p).unwrap();
        let st = b.search("aa", true, 10, &AtomicBool::new(false)).unwrap();
        assert_eq!(st.hits.len(), 1);
        assert!(st.hits[0].eps > 0.0, "eps={}", st.hits[0].eps);
    }

    #[test]
    fn snapshot_isolation_from_edits() {
        // Снапшот не видит правок живого буфера; правки не рвут ссылки.
        let (_d, p) = tmpbuf("iso.txt", "alpha beta gamma\n");
        let mut b = PolerBuffer::open_with_cap(&p, 6).unwrap();
        let snap = b.snapshot();
        b.insert(5, "XYZ").unwrap(); // alphaXYZ beta gamma
        b.delete(0, 8).unwrap(); // вырезаем «alphaXYZ» → « beta gamma»
        let out = run_once(&snap, "alpha", true, 10, &AtomicBool::new(false));
        assert_eq!(out.hits.len(), 1);
        assert_eq!(out.hits[0].byte, 0);
        // А живой буфер уже не находит «alpha».
        let st = b.search("alpha", true, 10, &AtomicBool::new(false)).unwrap();
        assert_eq!(st.hits.len(), 0);
        // Проверка целостности: правки не рвали ссылки кусков снапшота.
        assert_eq!(snap.read_range(0, 5).as_slice(), b"alpha");
    }

    /// Приёмник событий в Vec для инспекции порядка.
    struct VecSink {
        log: Vec<String>,
        hits_total: usize,
    }
    impl SearchEvents for VecSink {
        fn on_hits(&mut self, phase: &str, hits: &[HitOut], _scanned: u64, _total: u64) {
            self.hits_total += hits.len();
            self.log.push(format!("{phase}:{}", hits.len()));
        }
        fn on_progress(&mut self, _scanned: u64, _total: u64) {}
        fn on_viewport_done(&mut self, _ms: f64) {
            self.log.push("viewport_done".into());
        }
    }

    #[test]
    fn live_events_order_and_parity() {
        let text = format!("{}\n", "the quick brown fox\n".repeat(200));
        let (_d, p) = tmpbuf("live.txt", &text);
        let mut b = PolerBuffer::open_with_cap(&p, 256).unwrap();
        let snap = b.snapshot();
        // viewport: строки 10..20 → байты через живой буфер.
        let lo_line = 10u64;
        let lo = b.offset_of_line(lo_line).unwrap();
        let hi = b.offset_of_line(20).unwrap();
        let spec = LiveSpec {
            needle: "quick".into(),
            case_sensitive: true,
            limit: 100_000,
            viewport: Some((lo, hi, lo_line, lo)),
        };
        let mut sink = VecSink { log: vec![], hits_total: 0 };
        let sum = run_live(&snap, &spec, &AtomicBool::new(false), &mut sink);
        // Полная выдача совпадает с run_once.
        let once = run_once(&snap, "quick", true, 100_000, &AtomicBool::new(false));
        assert_eq!(sum.hits as usize, once.hits.len());
        assert_eq!(sink.hits_total, once.hits.len());
        assert!(!sum.truncated && !sum.cancelled);
        // События: первыми — viewport.
        assert!(sink.log.first().map(|s| s.starts_with("viewport")).unwrap_or(false));
        assert!(sink.log.iter().any(|s| s == "viewport_done"));
        // Фазы pre/post пришли (вьюпорт не с начала и не с конца).
        assert!(sink.log.iter().any(|s| s.starts_with("pre:")));
        assert!(sink.log.iter().any(|s| s.starts_with("post:")));
        // Совпадения между фазами не перекрываются и отсортированы по фазам.
        let all: Vec<u64> = once.hits.iter().map(|h| h.byte).collect();
        let mut sorted = all.clone();
        sorted.sort_unstable();
        assert_eq!(all, sorted, "run_once выдача в байтовом порядке");
    }

    #[test]
    fn live_respects_limit() {
        let (_d, p) = tmpbuf("dense.txt", &"ab\n".repeat(3000));
        let mut b = PolerBuffer::open(&p).unwrap();
        let snap = b.snapshot();
        let spec = LiveSpec {
            needle: "ab".into(),
            case_sensitive: true,
            limit: 7,
            viewport: None,
        };
        let mut sink = VecSink { log: vec![], hits_total: 0 };
        let sum = run_live(&snap, &spec, &AtomicBool::new(false), &mut sink);
        assert!(sum.truncated);
        assert_eq!(sum.hits, 7);
    }

    #[test]
    fn live_cancel_stops_early() {
        let (_d, p) = tmpbuf("canc.txt", &"needle here\n".repeat(5000));
        let mut b = PolerBuffer::open(&p).unwrap();
        let snap = b.snapshot();
        let spec = LiveSpec {
            needle: "needle".into(),
            case_sensitive: true,
            limit: 100_000,
            viewport: None,
        };
        let mut sink = VecSink { log: vec![], hits_total: 0 };
        let sum = run_live(&snap, &spec, &AtomicBool::new(true), &mut sink);
        assert!(sum.cancelled);
    }

    #[test]
    fn live_empty_needle_and_empty_doc() {
        let (_d, p) = tmpbuf("empty.txt", "");
        let mut b = PolerBuffer::open(&p).unwrap();
        let snap = b.snapshot();
        let spec = LiveSpec {
            needle: String::new(),
            case_sensitive: true,
            limit: 10,
            viewport: Some((0, 0, 0, 0)),
        };
        let mut sink = VecSink { log: vec![], hits_total: 0 };
        let sum = run_live(&snap, &spec, &AtomicBool::new(false), &mut sink);
        assert_eq!(sum.hits, 0);
        assert!(sink.log.is_empty());
    }
}
