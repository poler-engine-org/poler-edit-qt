# poler-edit-qt — GUI-клиент POLER Editor

Kate-подобный интерфейс поверх суверенного ядра `poler-edit`
(`poler-engine --edit-serve`). Зависимости: **только Qt6 Widgets** —
никаких KF6/KParts/Electron. Вся работа с текстом любого размера
происходит в ядре; GUI рендерит только видимое окно строк.

## Сборка (Arch Linux)

```bash
sudo pacman -S --needed base-devel cmake qt6-base
cd integrations/poler-edit-qt
cmake -B build -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/usr
cmake --build build -j$(nproc)
./build/poler-edit            # без установки
sudo cmake --install build    # системно: /usr/bin/poler-edit
```

Требование: `poler-engine` (v0.38.0+) в `PATH` (у пользователя он в
`~/.local/bin`). Переопределить путь можно переменной `POLER_ENGINE=/путь`.

## Запуск

```bash
poler-edit file.txt huge.log another.md   # несколько вкладок
poler-edit --light                        # светлая тема (по умолчанию тёмная, Breeze-стиль)
```

## Управление

| Действие | Клавиши |
|---|---|
| Открыть / Сохранить | Ctrl+O / Ctrl+S |
| Отмена / Возврат | Ctrl+Z / Ctrl+Y |
| Копировать / Вырезать / Вставить | Ctrl+C / Ctrl+X / Ctrl+V |
| Выделить всё | Ctrl+A |
| Найти (SIMD, весь файл любого размера) | Ctrl+F, далее Enter/F3 |
| vi-командная строка | `:` или Ctrl+: |
| Сохранить и закрыть | `:w` `:q` `:wq` `:q!` |
| Перейти к строке N | `:123` |
| Масштаб | Ctrl+колесо, Ctrl+= / Ctrl+- |
| Скрыть поиск / командную строку | Esc |

Во время фоновой индексации строк в статусной строке видна живая
скорость SIMD-подсчёта (GiB/s) — индексация не блокирует редактирование:
вьюпорт и правки доступны сразу после открытия.

## Архитектура

```
poler-edit (Qt6, тонкий клиент)
   │  JSON lines over stdio (LSP-стиль)
   ▼
poler-engine --edit-serve
   │  zero-copy mmap piece-table + SIMD line index + Aho-Corasick
   ▼
файл любого размера (RAM не зависит от объёма)
```

Протокол и цифры ядра: `docs/POLER_EDIT.md`.

## Ограничения v1 (честно)

- Подсветка синтаксиса — в планах (KSyntaxHighlighting XML / tree-sitter).
- save_as из GUI сохраняет в текущий путь (команда `save_as` в протоколе уже есть).
- Сложный IME-ввод (CJK) не тестировался; кириллица/латиница работают.
- Замену файла на диске во время редактирования ядро пока не отслеживает.
