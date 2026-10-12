#include "EditorView.h"

#include <QApplication>
#include <QClipboard>
#include <QFileInfo>
#include <QFontDatabase>
#include <QGuiApplication>
#include <QJsonArray>
#include <QKeyEvent>
#include <QPainter>
#include <QScrollBar>
#include <QtMath>

static constexpr int kOverscan = 40;      // строк сверх видимого окна
static constexpr int kBlinkMs = 530;
static constexpr int kGutterPad = 12;
// Пока line-index не готов, высота скроллбара оценивается по плотности строк.
static constexpr double kEstBytesPerLine = 96.0;

EditorView::EditorView(EngineBridge *bridge, const QString &path, QWidget *parent)
    : QAbstractScrollArea(parent)
    , m_bridge(bridge)
    , m_path(path)
{
    m_font = QFontDatabase::systemFont(QFontDatabase::FixedFont);
    m_font.setStyleHint(QFont::Monospace);
    if (m_font.pointSize() <= 0) {
        m_font.setPointSize(10);
    }
    m_baseSize = m_font.pointSize();
    setFont(m_font);

    setFocusPolicy(Qt::StrongFocus);
    viewport()->setCursor(Qt::IBeamCursor);
    setFrameShape(QFrame::NoFrame);
    viewport()->setAttribute(Qt::WA_OpaquePaintEvent);

    m_blink.setInterval(kBlinkMs);
    connect(&m_blink, &QTimer::timeout, this, &EditorView::blinkCursor);
    m_blink.start();

    connect(m_bridge, &EngineBridge::progressEvent, this, &EditorView::onProgress);

    // Скроллбары: коннект ОДИН раз (UniqueConnection не работает с лямбдами).
    connect(verticalScrollBar(), &QScrollBar::valueChanged, this, [this](int) {
        requestFetch(topLine());
        viewport()->update();
    });
    connect(horizontalScrollBar(), &QScrollBar::valueChanged, this, [this](int) {
        viewport()->update();
    });

    horizontalScrollBar()->setSingleStep(charWidth());
    openDoc();
}

QString EditorView::fileName() const
{
    if (m_path.isEmpty()) {
        return tr("без назви");
    }
    return QFileInfo(m_path).fileName();
}

// ---------------- геометрия ----------------

int EditorView::lineHeight() const
{
    return fontMetrics().height();
}

int EditorView::charWidth() const
{
    return fontMetrics().horizontalAdvance(QLatin1Char('M'));
}

int EditorView::gutterWidth() const
{
    const qlonglong maxLine = m_linesTotal > 0 ? m_linesTotal : qMax<qlonglong>(topLine() + visibleLines() + 1, 100);
    const int digits = qMax(2, int(qLn(qreal(qMax<qlonglong>(maxLine, 1)))) + 1);
    return digits * charWidth() + kGutterPad * 2;
}

int EditorView::visibleLines() const
{
    return qMax(1, viewport()->height() / lineHeight());
}

qlonglong EditorView::topLine() const
{
    return verticalScrollBar()->value();
}

void EditorView::setTopLine(qlonglong line)
{
    verticalScrollBar()->setValue(int(line));
}

// ---------------- кэш окна ----------------

QString EditorView::lineText(qlonglong line) const
{
    const qlonglong idx = line - m_blockTop;
    if (idx >= 0 && idx < m_lines.size()) {
        return m_lines.at(int(idx));
    }
    return QString();
}

qlonglong EditorView::lineLen(qlonglong line) const
{
    return lineText(line).length();
}

void EditorView::requestFetch(qlonglong aroundLine)
{
    fetchRange(qMax<qlonglong>(aroundLine - kOverscan / 2, 0),
               visibleLines() + kOverscan);
}

void EditorView::fetchRange(qlonglong top, int count)
{
    if (m_doc == 0) {
        return;
    }
    const qlonglong gen = ++m_fetchGen;
    m_bridge->viewport(m_doc, top, count, [this, gen, top](const QJsonObject &r) {
        if (gen != m_fetchGen || !r.value("ok").toBool()) {
            return; // пришёл устаревший запрос — молча отбрасываем
        }
        const QJsonArray arr = r.value("lines").toArray();
        m_lines.clear();
        m_lines.reserve(arr.size());
        for (const QJsonValue &v : arr) {
            m_lines.append(v.toObject().value("text").toString());
        }
        m_blockTop = top;
        if (gen > m_appliedGen) {
            m_appliedGen = gen;
        }
        viewport()->update();
    });
}

// ---------------- открытие ----------------

void EditorView::openDoc()
{
    m_bridge->open(m_path, [this](const QJsonObject &r) {
        if (!r.value("ok").toBool()) {
            emit statusMessage(tr("Не вдалося відкрити: %1")
                                   .arg(r.value("error").toString()), 6000);
            emit requestClose(this);
            return;
        }
        m_doc = r.value("doc").toVariant().toLongLong();
        m_bytesTotal = r.value("bytes").toVariant().toLongLong();
        const QJsonValue lines = r.value("lines");
        if (lines.isDouble()) {
            m_linesTotal = lines.toVariant().toLongLong();
        } else {
            // ленивое ядро: строк ещё не знаем — индексируем в фоне,
            // вьюпорт уже можно показывать (верх файла мгновенно готов).
            m_linesTotal = 0;
            m_indexing = true;
            m_bridge->indexDoc(m_doc, [this](const QJsonObject &r) {
                m_indexing = false;
                if (r.value("ok").toBool() && r.value("completed").toBool()) {
                    m_linesTotal = r.value("lines").toVariant().toLongLong();
                    updateScrollbars();
                    viewport()->update();
                    emit statusMessage(
                        tr("Індексація завершена: %1 рядків").arg(m_linesTotal), 4000);
                }
            });
        }
        updateScrollbars();
        setTopLine(0);
        requestFetch(0);
        emit docOpened(this);
    });
}

void EditorView::refreshInfo()
{
    if (m_doc == 0) {
        return;
    }
    // stats не нужен отдельной командой — байты приходят из правок
}

void EditorView::onProgress(const QJsonObject &obj)
{
    if (obj.value("doc").toVariant().toLongLong() != m_doc
        || obj.value("op").toString() != QLatin1String("index")) {
        return;
    }
    if (!m_indexing) {
        return;
    }
    const double done = obj.value("done_bytes").toDouble();
    const double total = obj.value("total_bytes").toDouble();
    const double gbps = obj.value("gbps").toDouble();
    auto human = [](double b) {
        if (b >= (1ull << 30)) return QString::number(b / (1ull << 30), 'f', 1) + " GiB";
        if (b >= (1ull << 20)) return QString::number(b / (1ull << 20), 'f', 0) + " MiB";
        return QString::number(b / (1ull << 10), 'f', 0) + " KiB";
    };
    emit statusMessage(tr("Індексація SIMD: %1 / %2 (%3 GiB/s)")
                           .arg(human(done), human(total))
                           .arg(gbps, 0, 'f', 1),
                       0);
}

// ---------------- скроллбары ----------------

void EditorView::updateScrollbars()
{
    qlonglong range;
    if (m_linesTotal > 0) {
        range = m_linesTotal;
    } else if (m_bytesTotal > 0) {
        range = qMax<qlonglong>(m_bytesTotal / kEstBytesPerLine, visibleLines());
    } else {
        range = 1;
    }
    QScrollBar *v = verticalScrollBar();
    v->setRange(0, int(qMax<qlonglong>(range - visibleLines() + 1, 1)));
    v->setPageStep(visibleLines());
    v->setSingleStep(1);
    horizontalScrollBar()->setRange(0, 4000);
    viewport()->update();
}

// ---------------- рендер ----------------

void EditorView::paintEvent(QPaintEvent *)
{
    QPainter p(viewport());
    const int lh = lineHeight();
    const int cw = charWidth();
    const int gw = gutterWidth();
    const int hscroll = horizontalScrollBar()->value() * cw;
    const int w = viewport()->width();
    const int h = viewport()->height();
    const int n = visibleLines() + 1;

    // Палитра (учитывает тёмную тему приложения)
    const QPalette pal = this->palette();
    const QColor bg = pal.color(QPalette::Base);
    const QColor fg = pal.color(QPalette::Text);
    const QColor gutter = pal.color(QPalette::PlaceholderText);
    const QColor curLineBg = pal.color(QPalette::AlternateBase);
    const QColor selBg = pal.color(QPalette::Highlight);

    p.fillRect(0, 0, w, h, bg);

    // gutter-фон чуть темнее
    p.fillRect(0, 0, gw, h, pal.color(QPalette::Window));

    const Pos selStart = qMin(m_anchor, m_cursor);
    const Pos selEnd = qMax(m_anchor, m_cursor);

    for (int i = 0; i < n; ++i) {
        const qlonglong lineNo = topLine() + i;
        if (m_linesTotal > 0 && lineNo >= m_linesTotal) {
            break;
        }
        const int y = i * lh;
        if (y > h) {
            break;
        }
        const QString text = lineText(lineNo);

        // подсветка текущей строки
        if (lineNo == m_cursor.line) {
            p.fillRect(gw, y, w - gw, lh, curLineBg);
        }
        // выделение
        if (hasSelection() && lineNo >= selStart.line && lineNo <= selEnd.line) {
            int fromCol = 0;
            int toCol = text.length();
            if (lineNo == selStart.line) {
                fromCol = int(qMin<qlonglong>(selStart.col, toCol));
            }
            if (lineNo == selEnd.line) {
                toCol = int(qMin<qlonglong>(selEnd.col, toCol));
            }
            if (toCol > fromCol || selStart.line != selEnd.line) {
                const int x0 = gw + fromCol * cw - hscroll;
                const int x1 = lineNo == selEnd.line ? gw + toCol * cw - hscroll : w;
                p.fillRect(qMax(x0, gw), y + 1,
                           qMax(x1, gw + cw) - qMax(x0, gw), lh - 2, selBg);
            }
        }

        // номер строки
        p.setPen(gutter);
        const QString num = QString::number(lineNo + 1);
        p.drawText(QRect(0, y, gw - kGutterPad, lh),
                   Qt::AlignRight | Qt::AlignVCenter, num);

        // текст
        p.setPen(fg);
        if (!text.isEmpty()) {
            const int skip = hscroll / cw;
            if (skip < text.length()) {
                p.drawText(QPoint(gw - (hscroll % cw),
                                  y + fontMetrics().ascent() + (lh - fontMetrics().height()) / 2),
                           text.mid(skip, w / cw + 2));
            }
        } else if (lineNo == m_cursor.line && !hasSelection()) {
            // пустая строка с курсором — рисуем и так (ниже)
        }
        if (lineNo == m_cursor.line && m_cursorVisible && hasFocus()) {
            const int cx = gw + int(m_cursor.col) * cw - hscroll;
            if (cx >= gw - 2) {
                p.fillRect(cx, y + 1, qMax(1, cw / 8), lh - 2, fg);
            }
        }
    }
    if (m_lines.isEmpty() && m_doc != 0) {
        p.setPen(gutter);
        p.drawText(QRect(gw, 0, w - gw, h), Qt::AlignCenter,
                   m_indexing ? tr("Індексація…") : tr("Завантаження…"));
    }
    p.end();
}

// ---------------- курсор ----------------

EditorView::Pos EditorView::clampPos(const Pos &p) const
{
    Pos r = p;
    if (m_linesTotal > 0) {
        r.line = qBound<qlonglong>(0, r.line, m_linesTotal - 1);
    } else {
        r.line = qMax<qlonglong>(r.line, 0);
    }
    const qlonglong len = lineLen(r.line);
    r.col = qBound<qlonglong>(0, r.col, len);
    return r;
}

void EditorView::setCursor(const Pos &p, bool keepAnchor)
{
    const Pos np = clampPos(p);
    if (!keepAnchor) {
        m_anchor = np;
    }
    m_cursor = np;
    m_cursorVisible = true;
    m_blink.start();
    ensureCursorVisible();
    emit cursorMoved(np.line + 1, np.col + 1);
    viewport()->update();
}

void EditorView::ensureCursorVisible()
{
    const qlonglong top = topLine();
    const int vis = visibleLines();
    if (m_cursor.line < top) {
        setTopLine(m_cursor.line);
        requestFetch(m_cursor.line);
    } else if (m_cursor.line >= top + vis - 1) {
        setTopLine(m_cursor.line - vis + 2);
        requestFetch(topLine());
    }
    const int cw = charWidth();
    const int gw = gutterWidth();
    const int cx = gw + int(m_cursor.col) * cw - horizontalScrollBar()->value() * cw;
    if (cx < gw) {
        horizontalScrollBar()->setValue(horizontalScrollBar()->value()
                                        - (gw - cx) / cw - 1);
    } else if (cx > viewport()->width() - cw * 4) {
        horizontalScrollBar()->setValue(horizontalScrollBar()->value()
                                        + (cx - viewport()->width()) / cw + 4);
    }
}

// ---------------- правки ----------------

void EditorView::insertText(const QString &text)
{
    if (m_doc == 0 || text.isEmpty()) {
        return;
    }
    const Pos at = clampPos(m_cursor);
    m_bridge->insertAt(m_doc, at.line, at.col, text, [this](const QJsonObject &) {
        afterEdit();
    });
    // Оптимистичный ход курсора: отзывчивость как у локального редактора.
    int nl = 0;
    int lastNl = -1;
    for (int i = 0; i < text.size(); ++i) {
        if (text.at(i) == QLatin1Char('\n')) {
            ++nl;
            lastNl = i;
        }
    }
    Pos np;
    if (nl > 0) {
        np.line = at.line + nl;
        np.col = text.size() - lastNl - 1;
    } else {
        np.line = at.line;
        np.col = at.col + text.size();
    }
    m_anchor = np;
    m_cursor = np;
    ensureCursorVisible();
    if (!m_modified) {
        m_modified = true;
        emit docModified(this, true);
    }
    viewport()->update();
}

void EditorView::afterEdit()
{
    requestFetch(m_cursor.line);
    viewport()->update();
}

void EditorView::deleteSelection()
{
    if (!hasSelection() || m_doc == 0) {
        return;
    }
    const Pos s = qMin(m_anchor, m_cursor);
    const Pos e = qMax(m_anchor, m_cursor);
    m_bridge->del(m_doc, s.line, s.col, e.line, e.col, [this](const QJsonObject &) {
        afterEdit();
    });
    m_anchor = s;
    m_cursor = s;
    ensureCursorVisible();
    if (!m_modified) {
        m_modified = true;
        emit docModified(this, true);
    }
    viewport()->update();
}

void EditorView::undo()
{
    if (m_doc == 0) {
        return;
    }
    m_bridge->undo(m_doc, [this](const QJsonObject &r) {
        if (r.value("applied").toBool()) {
            m_modified = true;
            emit docModified(this, true);
        }
        requestFetch(m_cursor.line);
        viewport()->update();
    });
}

void EditorView::redo()
{
    if (m_doc == 0) {
        return;
    }
    m_bridge->redo(m_doc, [this](const QJsonObject &r) {
        if (r.value("applied").toBool()) {
            m_modified = true;
            emit docModified(this, true);
        }
        requestFetch(m_cursor.line);
        viewport()->update();
    });
}

// ---------------- буфер обмена / выделение ----------------

QString EditorView::selectedText() const
{
    if (!hasSelection()) {
        return QString();
    }
    const Pos s = qMin(m_anchor, m_cursor);
    const Pos e = qMax(m_anchor, m_cursor);
    QString out;
    for (qlonglong l = s.line; l <= e.line; ++l) {
        const QString text = lineText(l);
        int a = 0;
        int b = text.length();
        if (l == s.line) {
            a = int(qMin<qlonglong>(s.col, b));
        }
        if (l == e.line) {
            b = int(qMin<qlonglong>(e.col, b));
        }
        out += text.mid(a, qMax(0, b - a));
        if (l < e.line) {
            out += QLatin1Char('\n');
        }
    }
    return out;
}

void EditorView::copy()
{
    const QString sel = selectedText();
    if (!sel.isEmpty()) {
        QGuiApplication::clipboard()->setText(sel);
        emit statusMessage(tr("Скопійовано %n симв.", nullptr, sel.size()), 2000);
    }
}

void EditorView::cut()
{
    if (hasSelection()) {
        copy();
        deleteSelection();
    }
}

void EditorView::paste()
{
    const QString text = QGuiApplication::clipboard()->text();
    if (!text.isEmpty()) {
        if (hasSelection()) {
            deleteSelection();
        }
        insertText(text);
    }
}

void EditorView::selectAll()
{
    m_anchor = Pos{0, 0};
    const qlonglong last = m_linesTotal > 0 ? m_linesTotal - 1 : topLine() + visibleLines();
    m_cursor = Pos{last, lineLen(last)};
    ensureCursorVisible();
    viewport()->update();
}

// ---------------- клавиатура ----------------

void EditorView::keyPressEvent(QKeyEvent *e)
{
    const Qt::KeyboardModifiers mod = e->modifiers();
    const bool shift = mod & Qt::ShiftModifier;
    const bool ctrl = mod & Qt::ControlModifier;

    if (e->key() == Qt::Key_Escape) {
        m_anchor = m_cursor;
        viewport()->update();
        return;
    }
    // vi-стиль: ':' открывает командную строку (text() надёжнее key():
    // на многих раскладках двоеточие требует Shift).
    if (!ctrl && !(mod & Qt::AltModifier) && e->text() == QStringLiteral(":")) {
        emit commandLineRequested();
        return;
    }

    switch (e->key()) {
    case Qt::Key_Up:
        setCursor(Pos{m_cursor.line - 1, m_cursor.col}, !shift);
        return;
    case Qt::Key_Down:
        setCursor(Pos{m_cursor.line + 1, m_cursor.col}, !shift);
        return;
    case Qt::Key_Left:
        if (m_cursor.col > 0) {
            setCursor(Pos{m_cursor.line, m_cursor.col - 1}, !shift);
        } else if (m_cursor.line > 0) {
            setCursor(Pos{m_cursor.line - 1, lineLen(m_cursor.line - 1)}, !shift);
        }
        return;
    case Qt::Key_Right:
        if (m_cursor.col < lineLen(m_cursor.line)) {
            setCursor(Pos{m_cursor.line, m_cursor.col + 1}, !shift);
        } else {
            setCursor(Pos{m_cursor.line + 1, 0}, !shift);
        }
        return;
    case Qt::Key_Home:
        setCursor(Pos{m_cursor.line, 0}, !shift);
        return;
    case Qt::Key_End:
        setCursor(Pos{m_cursor.line, lineLen(m_cursor.line)}, !shift);
        return;
    case Qt::Key_PageUp:
        setCursor(Pos{m_cursor.line - visibleLines() + 1, m_cursor.col}, !shift);
        return;
    case Qt::Key_PageDown:
        setCursor(Pos{m_cursor.line + visibleLines() - 1, m_cursor.col}, !shift);
        return;
    case Qt::Key_Return:
    case Qt::Key_Enter:
        if (hasSelection()) {
            deleteSelection();
        }
        insertText(QStringLiteral("\n"));
        return;
    case Qt::Key_Backspace:
        if (hasSelection()) {
            deleteSelection();
        } else if (m_cursor.col > 0) {
            const Pos s{m_cursor.line, m_cursor.col - 1};
            m_bridge->del(m_doc, s.line, s.col, m_cursor.line, m_cursor.col,
                          [this](const QJsonObject &) { afterEdit(); });
            m_anchor = s;
            m_cursor = s;
            m_modified = true;
            emit docModified(this, true);
            ensureCursorVisible();
            viewport()->update();
        } else if (m_cursor.line > 0) {
            const qlonglong prevLen = lineLen(m_cursor.line - 1);
            m_bridge->del(m_doc, m_cursor.line - 1, prevLen, m_cursor.line, 0,
                          [this](const QJsonObject &) { afterEdit(); });
            const Pos s{m_cursor.line - 1, prevLen};
            m_anchor = s;
            m_cursor = s;
            m_modified = true;
            emit docModified(this, true);
            ensureCursorVisible();
            viewport()->update();
        }
        return;
    case Qt::Key_Delete:
        if (hasSelection()) {
            deleteSelection();
        } else if (m_cursor.col < lineLen(m_cursor.line)) {
            m_bridge->del(m_doc, m_cursor.line, m_cursor.col,
                          m_cursor.line, m_cursor.col + 1,
                          [this](const QJsonObject &) { afterEdit(); });
            m_modified = true;
            emit docModified(this, true);
            viewport()->update();
        } else {
            // Join со следующей строкой (удаляем \n)
            m_bridge->del(m_doc, m_cursor.line, m_cursor.col,
                          m_cursor.line + 1, 0,
                          [this](const QJsonObject &) { afterEdit(); });
            m_modified = true;
            emit docModified(this, true);
            viewport()->update();
        }
        return;
    case Qt::Key_Tab:
        if (hasSelection()) {
            deleteSelection();
        }
        insertText(QStringLiteral("    "));
        return;
    default:
        break;
    }

    if (ctrl) {
        QAbstractScrollArea::keyPressEvent(e);
        return;
    }

    const QString text = e->text();
    if (!text.isEmpty() && !text.contains(QLatin1Char('\r'))) {
        if (hasSelection()) {
            deleteSelection();
        }
        insertText(text);
        return;
    }
    QAbstractScrollArea::keyPressEvent(e);
}

bool EditorView::event(QEvent *e)
{
    if (e->type() == QEvent::ToolTip) {
        return QAbstractScrollArea::event(e);
    }
    return QAbstractScrollArea::event(e);
}

// ---------------- мышь ----------------

void EditorView::mousePressEvent(QMouseEvent *e)
{
    if (e->button() != Qt::LeftButton) {
        QAbstractScrollArea::mousePressEvent(e);
        return;
    }
    setFocus();
    const int lh = lineHeight();
    const int cw = charWidth();
    const int gw = gutterWidth();
    const qlonglong line = topLine() + e->pos().y() / lh;
    const qlonglong col = qMax<qlonglong>(0, (e->pos().x() - gw + horizontalScrollBar()->value() * cw) / cw);
    setCursor(Pos{line, col}, e->modifiers() & Qt::ShiftModifier);
}

void EditorView::mouseMoveEvent(QMouseEvent *e)
{
    if (e->buttons() & Qt::LeftButton) {
        const int lh = lineHeight();
        const int cw = charWidth();
        const int gw = gutterWidth();
        const qlonglong line = topLine() + e->pos().y() / lh;
        const qlonglong col = qMax<qlonglong>(0, (e->pos().x() - gw + horizontalScrollBar()->value() * cw) / cw);
        // keepAnchor = true для непрерывного выделения фрагментов мышью
        setCursor(Pos{line, col}, true);
    } else {
        QAbstractScrollArea::mouseMoveEvent(e);
    }
}

void EditorView::mouseReleaseEvent(QMouseEvent *e)
{
    if (e->button() == Qt::LeftButton) {
        // Завершение выделения мышью
        viewport()->update();
    } else {
        QAbstractScrollArea::mouseReleaseEvent(e);
    }
}

void EditorView::mouseDoubleClickEvent(QMouseEvent *e)
{
    // v1: двойной клик выделяет слово (по пробелам/знакам)
    const int cw = charWidth();
    const int gw = gutterWidth();
    const qlonglong line = topLine() + e->pos().y() / lineHeight();
    const qlonglong col = qMax<qlonglong>(0, (e->pos().x() - gw + horizontalScrollBar()->value() * cw) / cw);
    const QString text = lineText(line);
    if (text.isEmpty()) {
        return;
    }
    const int c = int(qMin<qlonglong>(col, text.length() - 1));
    const auto isWord = [](QChar ch) {
        return ch.isLetterOrNumber() || ch == QLatin1Char('_');
    };
    int a = c;
    int b = c;
    while (a > 0 && isWord(text.at(a - 1))) {
        --a;
    }
    while (b < text.length() - 1 && isWord(text.at(b + 1))) {
        ++b;
    }
    m_anchor = Pos{line, a};
    m_cursor = Pos{line, b + 1};
    viewport()->update();
}

void EditorView::wheelEvent(QWheelEvent *e)
{
    const int steps = e->angleDelta().y() / 120;
    if (e->modifiers() & Qt::ControlModifier) {
        if (steps > 0) {
            zoomIn();
        } else {
            zoomOut();
        }
        return;
    }
    verticalScrollBar()->setValue(verticalScrollBar()->value() - steps * 3);
    requestFetch(topLine());
    viewport()->update();
}

void EditorView::resizeEvent(QResizeEvent *e)
{
    QAbstractScrollArea::resizeEvent(e);
    updateScrollbars();
    requestFetch(topLine());
}

void EditorView::focusInEvent(QFocusEvent *e)
{
    QAbstractScrollArea::focusInEvent(e);
    m_cursorVisible = true;
    m_blink.start();
    viewport()->update();
}

void EditorView::blinkCursor()
{
    if (hasFocus()) {
        m_cursorVisible = !m_cursorVisible;
        viewport()->update();
    }
}

void EditorView::zoomIn()
{
    m_zoom = qMin(8, m_zoom + 1);
    m_font.setPointSize(qMax(6, m_baseSize + m_zoom));
    setFont(m_font);
    updateScrollbars();
    requestFetch(topLine());
}

void EditorView::zoomOut()
{
    m_zoom = qMax(-6, m_zoom - 1);
    m_font.setPointSize(qMax(6, m_baseSize + m_zoom));
    setFont(m_font);
    updateScrollbars();
    requestFetch(topLine());
}

// ---------------- поиск ----------------

void EditorView::search(const QString &query, bool caseSensitive)
{
    if (m_doc == 0 || query.isEmpty()) {
        return;
    }
    m_lastQuery = query;
    emit statusMessage(tr("Пошук «%1»…").arg(query), 0);
    m_bridge->search(m_doc, query, caseSensitive, 5000, [this, query](const QJsonObject &r) {
        if (!r.value("ok").toBool()) {
            emit statusMessage(tr("Помилка пошуку: %1").arg(r.value("error").toString()), 5000);
            return;
        }
        m_hits.clear();
        const QJsonArray arr = r.value("hits").toArray();
        for (const QJsonValue &v : arr) {
            const QJsonObject h = v.toObject();
            PolarHit hit;
            hit.line = h.value("line").toVariant().toLongLong();
            hit.col = h.value("col").toVariant().toLongLong();
            hit.len = h.value("len").toVariant().toLongLong();
            hit.byte = h.value("byte").toVariant().toLongLong();
            m_hits.append(hit);
        }
        m_hitIndex = 0;
        if (m_hits.isEmpty()) {
            emit statusMessage(tr("«%1»: збігів немає").arg(query), 4000);
        } else {
            jumpToHit(0);
        }
    });
}

void EditorView::jumpToHit(int index)
{
    if (m_hits.isEmpty()) {
        return;
    }
    m_hitIndex = ((index % m_hits.size()) + m_hits.size()) % m_hits.size();
    const PolarHit &h = m_hits.at(m_hitIndex);
    setTopLine(qMax<qlonglong>(h.line - visibleLines() / 2, 0));
    requestFetch(h.line);
    setCursor(Pos{h.line, h.col});
    emit statusMessage(tr("Збіг %1 / %2").arg(m_hitIndex + 1).arg(m_hits.size()), 3000);
}

void EditorView::nextHit()
{
    if (m_hits.isEmpty()) {
        return;
    }
    jumpToHit(m_hitIndex + 1);
}

void EditorView::gotoLine(qlonglong line)
{
    // 1-based номер строки, как в vi (:123)
    const qlonglong l = qMax<qlonglong>(line - 1, 0);
    setTopLine(qMax<qlonglong>(l - visibleLines() / 2, 0));
    requestFetch(l);
    setCursor(Pos{l, 0});
    emit statusMessage(tr("Рядок %1").arg(l + 1), 2000);
}

// ---------------- сохранение ----------------

void EditorView::save(bool as)
{
    if (m_doc == 0) {
        return;
    }
    auto done = [this](const QJsonObject &r) {
        if (r.value("ok").toBool()) {
            m_modified = false;
            emit docModified(this, false);
            emit statusMessage(tr("Збережено: %1").arg(r.value("path").toString()), 3000);
        } else {
            emit statusMessage(tr("Помилка збереження: %1").arg(r.value("error").toString()), 6000);
        }
    };
    if (as || m_path.isEmpty()) {
        // TODO v1.1: диалог Save As — пока сохраняем как есть
        m_bridge->save(m_doc, done);
    } else {
        m_bridge->save(m_doc, done);
    }
}
