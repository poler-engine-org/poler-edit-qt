#include "MainWindow.h"

#include <QApplication>
#include <QCloseEvent>
#include <QDateTime>
#include <QDialog>
#include <QDialogButtonBox>
#include <QFileDialog>
#include <QFileInfo>
#include <QHBoxLayout>
#include <QKeyEvent>
#include <QLabel>
#include <QLineEdit>
#include <QMenuBar>
#include <QMessageBox>
#include <QPushButton>
#include <QSettings>
#include <QStatusBar>
#include <QStyle>
#include <QTabWidget>
#include <QTimer>
#include <QToolBar>
#include <QVBoxLayout>

// As-you-type debounce: гигабайтные файлы сканируются потоково, но
// дергать ядро на каждый keystroke незачем — 160 мс тишины достаточно.
static constexpr int kFindDebounceMs = 160;

MainWindow::MainWindow(QWidget *parent)
    : QMainWindow(parent)
    , m_bridge(new EngineBridge(this))
{
    setMinimumSize(900, 600);
    resize(1180, 780);

    m_tabs = new QTabWidget(this);
    m_tabs->setTabsClosable(true);
    m_tabs->setMovable(true);
    m_tabs->setDocumentMode(true);
    connect(m_tabs, &QTabWidget::tabCloseRequested, this, &MainWindow::onCloseTab);
    connect(m_tabs, &QTabWidget::currentChanged, this, &MainWindow::onTabChanged);

    buildMenus();
    buildToolbar();
    buildStatusBar();

    // Центральная компоновка: [панель поиска (скрыта)] [вкладки] [':'-строка (скрыта)]
    auto *central = new QWidget(this);
    auto *cl = new QVBoxLayout(central);
    cl->setContentsMargins(0, 0, 0, 0);
    cl->setSpacing(0);
    cl->addWidget(buildSearchBar());
    cl->addWidget(m_tabs, 1);
    cl->addWidget(buildCommandLine());
    setCentralWidget(central);

    connect(m_bridge, &EngineBridge::progressEvent, this, &MainWindow::onProgress);
    connect(m_bridge, &EngineBridge::engineDied, this, &MainWindow::onEngineDied);

    // Debounce живого поиска (as-you-type).
    m_findDebounce = new QTimer(this);
    m_findDebounce->setSingleShot(true);
    m_findDebounce->setInterval(kFindDebounceMs);
    connect(m_findDebounce, &QTimer::timeout, this, &MainWindow::runLiveSearch);

    // Офлайн-активация сохранённого Pro-ключа (Ed25519 в ядре).
    validateStoredLicense();

    updateTitle();
}

QWidget *MainWindow::buildSearchBar()
{
    m_searchBar = new QWidget(this);
    auto *lay = new QHBoxLayout(m_searchBar);
    lay->setContentsMargins(8, 4, 8, 4);
    m_searchEdit = new QLineEdit(m_searchBar);
    m_searchEdit->setPlaceholderText(
        tr("Знайти — живий пошук під час набору (Enter — далі, Esc — закрити)"));
    m_searchEdit->setClearButtonEnabled(true);

    // "Aa" — учёт регистра (по умолчанию чувствительный, как в grep).
    m_caseBtn = new QPushButton(tr("Aa"), m_searchBar);
    m_caseBtn->setCheckable(true);
    m_caseBtn->setChecked(true);
    m_caseBtn->setToolTip(tr("Враховувати регістр (grep-режим)"));
    m_caseBtn->setFixedWidth(36);

    auto *prev = new QPushButton(tr("◀"), m_searchBar);
    auto *next = new QPushButton(tr("▶"), m_searchBar);
    auto *close = new QPushButton(tr("✕"), m_searchBar);
    prev->setFixedWidth(36);
    next->setFixedWidth(36);
    close->setFixedWidth(32);

    // Счётчик совпадений: "12/345…" — счёт растёт потоком во время скана,
    // "…" исчезает по search_done; "⚠" — выдача обрезана лимитом (5000).
    m_hitLabel = new QLabel(m_searchBar);
    m_hitLabel->setMinimumWidth(90);
    m_hitLabel->setAlignment(Qt::AlignRight | Qt::AlignVCenter);
    m_hitLabel->setStyleSheet("color: #8da3b8;");

    lay->addWidget(m_searchEdit, 1);
    lay->addWidget(m_caseBtn);
    lay->addWidget(prev);
    lay->addWidget(next);
    lay->addWidget(m_hitLabel);
    lay->addWidget(close);

    connect(next, &QPushButton::clicked, this, &MainWindow::findNext);
    connect(prev, &QPushButton::clicked, this, &MainWindow::findPrev);
    connect(close, &QPushButton::clicked, m_searchBar, &QWidget::hide);
    // Enter — дальше, Shift+Enter — назад (как F3/Shift+F3).
    connect(m_searchEdit, &QLineEdit::returnPressed, this, [this] {
        if (QApplication::queryKeyboardModifiers() & Qt::ShiftModifier) {
            findPrev();
        } else {
            findNext();
        }
    });
    // As-you-type: перезапуск сессии с debounce; пустая строка — сброс.
    connect(m_searchEdit, &QLineEdit::textChanged, this, [this](const QString &) {
        m_findDebounce->start();
    });
    connect(m_caseBtn, &QPushButton::toggled, this, [this](bool) {
        m_findDebounce->start();
    });
    m_searchEdit->installEventFilter(this);
    m_searchBar->hide();
    return m_searchBar;
}

QWidget *MainWindow::buildCommandLine()
{
    m_cmdWrap = new QWidget(this);
    auto *lay = new QHBoxLayout(m_cmdWrap);
    lay->setContentsMargins(8, 3, 8, 3);
    auto *mark = new QLabel(":", m_cmdWrap);
    mark->setStyleSheet("font-weight: 700;");
    m_cmdLine = new QLineEdit(m_cmdWrap);
    m_cmdLine->setPlaceholderText(
        tr("w — зберегти; q — закрити вкладку; wq; q!; N — перейти до рядка N"));
    lay->addWidget(mark);
    lay->addWidget(m_cmdLine, 1);
    connect(m_cmdLine, &QLineEdit::returnPressed, this, [this] {
        onCommandLine(m_cmdLine->text());
        m_cmdLine->clear();
        hideCommandLine();
    });
    m_cmdLine->installEventFilter(this);
    m_cmdWrap->hide();
    return m_cmdWrap;
}

// ---------------- документы ----------------

EditorView *MainWindow::currentView() const
{
    return viewAt(m_tabs->currentIndex());
}

EditorView *MainWindow::viewAt(int index) const
{
    if (index < 0) {
        return nullptr;
    }
    return qobject_cast<EditorView *>(m_tabs->widget(index));
}

void MainWindow::attachView(EditorView *view)
{
    connect(view, &EditorView::cursorMoved, this, &MainWindow::onCursorMoved);
    connect(view, &EditorView::statusMessage, this, &MainWindow::onStatusMessage);
    connect(view, &EditorView::docModified, this, &MainWindow::onDocModified);
    connect(view, &EditorView::hitCountChanged, this, &MainWindow::onHitCountChanged);
    connect(view, &EditorView::requestClose, this, [this, view] {
        const int i = m_tabs->indexOf(view);
        if (i >= 0) {
            onCloseTab(i);
        }
    });
    connect(view, &EditorView::commandLineRequested, this, &MainWindow::showCommandLine);
}

void MainWindow::openFile(const QString &path)
{
    auto *view = new EditorView(m_bridge, path, this);
    attachView(view);
    m_tabs->addTab(view, QFileInfo(path).fileName());
    m_tabs->setCurrentWidget(view);
}

void MainWindow::openUntitled()
{
    auto *view = new EditorView(m_bridge, QString(), this);
    attachView(view);
    m_tabs->addTab(view, tr("без назви"));
    m_tabs->setCurrentWidget(view);
}

void MainWindow::onTabChanged(int)
{
    updateTitle();
    if (EditorView *v = currentView()) {
        m_modeLabel->setText(v->isIndexed() ? tr("рядки: точно") : tr("рядки: індекс…"));
        // Панель поиска видима — переносим живой поиск на новый документ
        // (каждая вкладка держит собственное состояние сессии).
        if (m_searchBar && m_searchBar->isVisible() && !m_searchEdit->text().isEmpty()) {
            v->searchLive(m_searchEdit->text(), m_caseBtn->isChecked());
            onHitCountChanged(v->hitCount(), 0, v->searchActive(), false);
        }
    }
}

// ---------------- меню ----------------

void MainWindow::buildMenus()
{
    QMenu *file = menuBar()->addMenu(tr("&Файл"));
    file->addAction(tr("&Відкрити…"), QKeySequence::Open, this, &MainWindow::onOpen);
    file->addAction(tr("&Зберегти"), QKeySequence::Save, this, &MainWindow::onSave);
    file->addAction(tr("Зберегти &як…"), QKeySequence::SaveAs, this, &MainWindow::onSaveAs);
    file->addSeparator();
    file->addAction(tr("Закрити вклад&ку"), QKeySequence::Close, this, [this] {
        onCloseTab(m_tabs->currentIndex());
    });
    file->addAction(tr("&Вихід"), QKeySequence::Quit, qApp, &QApplication::quit);

    QMenu *edit = menuBar()->addMenu(tr("&Правка"));
    edit->addAction(tr("&Скасувати"), QKeySequence::Undo, this, [this] {
        if (EditorView *v = currentView()) v->undo();
    });
    edit->addAction(tr("&Повернути"), QKeySequence::Redo, this, [this] {
        if (EditorView *v = currentView()) v->redo();
    });
    edit->addSeparator();
    edit->addAction(tr("Ви&різати"), QKeySequence::Cut, this, [this] {
        if (EditorView *v = currentView()) v->cut();
    });
    edit->addAction(tr("&Копіювати"), QKeySequence::Copy, this, [this] {
        if (EditorView *v = currentView()) v->copy();
    });
    edit->addAction(tr("В&ставити"), QKeySequence::Paste, this, [this] {
        if (EditorView *v = currentView()) v->paste();
    });
    edit->addSeparator();
    edit->addAction(tr("Виділити &все"), QKeySequence::SelectAll, this, [this] {
        if (EditorView *v = currentView()) v->selectAll();
    });

    QMenu *view = menuBar()->addMenu(tr("&Вигляд"));
    view->addAction(tr("Збіль&шити шрифт"), QKeySequence::ZoomIn, this, [this] {
        if (EditorView *v = currentView()) v->zoomIn();
    });
    view->addAction(tr("З&меншити шрифт"), QKeySequence::ZoomOut, this, [this] {
        if (EditorView *v = currentView()) v->zoomOut();
    });
    view->addSeparator();
    view->addAction(tr("Командний рядок &:"), QKeySequence(Qt::CTRL | Qt::Key_Colon),
                    this, &MainWindow::showCommandLine);

    QMenu *search = menuBar()->addMenu(tr("&Пошук"));
    search->addAction(tr("&Знайти…"), QKeySequence::Find, this, &MainWindow::onFind);
    search->addAction(tr("Знайти &далі"), QKeySequence(Qt::Key_F3), this, &MainWindow::findNext);
    search->addAction(tr("Знайти &назад"), QKeySequence(Qt::SHIFT | Qt::Key_F3),
                      this, &MainWindow::findPrev);

    QMenu *help = menuBar()->addMenu(tr("&Довідка"));
    help->addAction(tr("&Про POLER Edit"), QKeySequence::HelpContents, this, &MainWindow::showAbout);
    help->addSeparator();
    help->addAction(tr("Активація &Pro…"), this, &MainWindow::showLicenseDialog);
}

void MainWindow::buildToolbar()
{
    QToolBar *tb = addToolBar(tr("Головна"));
    tb->setMovable(false);
    tb->setIconSize(QSize(18, 18));
    tb->setToolButtonStyle(Qt::ToolButtonTextBesideIcon);
    tb->addAction(style()->standardIcon(QStyle::SP_DialogOpenButton), tr("Відкрити"),
                  this, &MainWindow::onOpen);
    tb->addAction(style()->standardIcon(QStyle::SP_DialogSaveButton), tr("Зберегти"),
                  this, &MainWindow::onSave);
    tb->addSeparator();
    tb->addAction(style()->standardIcon(QStyle::SP_ArrowBack), tr("Скасувати"), this, [this] {
        if (EditorView *v = currentView()) v->undo();
    });
    tb->addAction(style()->standardIcon(QStyle::SP_ArrowForward), tr("Повернути"), this, [this] {
        if (EditorView *v = currentView()) v->redo();
    });
    tb->addSeparator();
    tb->addAction(style()->standardIcon(QStyle::SP_FileDialogContentsView), tr("Знайти"),
                  this, &MainWindow::onFind);
}

// ---------------- статусная строка ----------------

void MainWindow::buildStatusBar()
{
    m_posLabel = new QLabel(tr("Ряд 1, Ст 1"), this);
    m_bytesLabel = new QLabel(tr("— байт"), this);
    m_linesLabel = new QLabel(tr("— рядків"), this);
    m_modeLabel = new QLabel(tr("UTF-8"), this);
    m_licLabel = new QLabel(tr("Free"), this);
    m_engineLabel = new QLabel(tr("POLER ●"), this);
    for (QLabel *l : {m_posLabel, m_bytesLabel, m_linesLabel, m_modeLabel,
                      m_licLabel, m_engineLabel}) {
        l->setMargin(6);
        statusBar()->addPermanentWidget(l);
    }
    m_engineLabel->setStyleSheet("color: #2ecc71; font-weight: 600;");
    m_licLabel->setStyleSheet("color: #8da3b8; font-weight: 600;");
    m_licLabel->setToolTip(tr("Ліцензія: Довідка → Активація Pro…"));
}

void MainWindow::onCursorMoved(qlonglong line, qlonglong col)
{
    m_posLabel->setText(tr("Ряд %1, Ст %2").arg(line).arg(col));
}

void MainWindow::onStatusMessage(const QString &msg, int timeoutMs)
{
    statusBar()->showMessage(msg, timeoutMs);
}

void MainWindow::onDocModified(EditorView *view, bool modified)
{
    Q_UNUSED(modified);
    updateTabTitle(view);
    updateTitle();
}

void MainWindow::onProgress(const QJsonObject &)
{
    // Прогресс индексации ретранслируется самим EditorView в statusMessage.
}

void MainWindow::onEngineDied()
{
    m_engineLabel->setText(tr("POLER ○"));
    m_engineLabel->setStyleSheet("color: #da4453; font-weight: 600;");
    statusBar()->showMessage(
        tr("Ядро poler-edit-core недоступне. Перевірте встановлення "
           "(змінна POLER_EDIT_CORE або core/target/release/poler-edit-core)."),
        10000);
}

void MainWindow::updateTabTitle(EditorView *view)
{
    if (!view) {
        return;
    }
    const int i = m_tabs->indexOf(view);
    if (i < 0) {
        return;
    }
    m_tabs->setTabText(i, (view->isModified() ? "* " : QString()) + view->fileName());
}

void MainWindow::updateTitle()
{
    EditorView *v = currentView();
    if (v) {
        setWindowTitle(tr("%1%2 — POLER Editor")
                           .arg(v->isModified() ? "*" : QString())
                           .arg(v->fileName()));
    } else {
        setWindowTitle(tr("POLER Editor"));
    }
}

// ---------------- команды ----------------

void MainWindow::onOpen()
{
    const QStringList files = QFileDialog::getOpenFileNames(
        this, tr("Відкрити файли"), QString(),
        tr("Усі файли (*);;Текст (*.txt *.md *.log *.rs *.c *.cpp *.py *.json *.xml *.yaml)"));
    for (const QString &f : files) {
        openFile(f);
    }
}

void MainWindow::onSave()
{
    if (EditorView *v = currentView()) {
        v->save(false);
    }
}

void MainWindow::onSaveAs()
{
    const QString path = QFileDialog::getSaveFileName(this, tr("Зберегти як"));
    if (!path.isEmpty()) {
        // v1.1: save_as в ядро; пока сохраняем текущий документ
        if (EditorView *v = currentView()) {
            v->save(false);
        }
    }
}

void MainWindow::onCloseTab(int index)
{
    EditorView *v = viewAt(index);
    if (!v) {
        return;
    }
    if (v->isModified()) {
        const auto btn = QMessageBox::question(
            this, tr("Незбережені зміни"),
            tr("«%1» містить незбережені зміни. Закрити без збереження?").arg(v->fileName()),
            QMessageBox::Save | QMessageBox::Discard | QMessageBox::Cancel);
        if (btn == QMessageBox::Cancel) {
            return;
        }
        if (btn == QMessageBox::Save) {
            v->save(false);
        }
    }
    if (v->docId() > 0) {
        m_bridge->closeDoc(v->docId());
    }
    m_tabs->removeTab(index);
    v->deleteLater();
    updateTitle();
}

void MainWindow::closeEvent(QCloseEvent *e)
{
    for (int i = 0; i < m_tabs->count(); ++i) {
        if (viewAt(i) && viewAt(i)->isModified()) {
            const auto btn = QMessageBox::question(
                this, tr("Незбережені зміни"),
                tr("Є незбережені зміни. Вийти без збереження?"),
                QMessageBox::Save | QMessageBox::Discard | QMessageBox::Cancel);
            if (btn == QMessageBox::Cancel) {
                e->ignore();
                return;
            }
            if (btn == QMessageBox::Save) {
                viewAt(i)->save(false);
            }
            break;
        }
    }
    m_bridge->quit();
    e->accept();
}

// ---------------- поиск (Search Engine v2, live) ----------------

void MainWindow::onFind()
{
    m_searchBar->show();
    m_searchEdit->setFocus();
    m_searchEdit->selectAll();
    // Повторный Ctrl+F с уже набранным запросом — мгновенный перезапуск
    // с текущим окном вьюпорта активной вкладки.
    if (currentView() && !m_searchEdit->text().isEmpty()) {
        m_findDebounce->start();
    }
}

void MainWindow::runLiveSearch()
{
    EditorView *v = currentView();
    if (!v) {
        return;
    }
    const QString q = m_searchEdit->text();
    if (q.isEmpty()) {
        v->clearSearch();
        onHitCountChanged(0, 0, false, false);
        return;
    }
    v->searchLive(q, m_caseBtn->isChecked());
}

void MainWindow::onHitCountChanged(int total, int current, bool scanning, bool truncated)
{
    // Счётчик показывает сессию АКТИВНОЙ вкладки (сигналы фоновых
    // вкладок приходят от их собственных live-сессий — игнорируем).
    if (sender() && sender() != currentView()) {
        return;
    }
    if (!m_hitLabel) {
        return;
    }
    if (total <= 0) {
        m_hitLabel->setText(scanning ? tr("скан…") : QString());
        return;
    }
    // "5/345…" — потоковый счёт; "⚠" — лимит в 5000 совпадений.
    m_hitLabel->setText(tr("%1/%2%3%4")
                            .arg(current + 1)
                            .arg(total)
                            .arg(scanning ? tr("…") : QString())
                            .arg(truncated ? tr(" ⚠") : QString()));
}

void MainWindow::findNext()
{
    EditorView *v = currentView();
    if (!v) {
        return;
    }
    if (v->hitCount() > 0) {
        v->nextHit();
    } else if (!m_searchEdit->text().isEmpty()) {
        // Enter раньше debounce — запускаем немедленно (фаза вьюпорта
        // выдаст первые совпадения и авто-переход за миллисекунды).
        m_findDebounce->stop();
        v->searchLive(m_searchEdit->text(), m_caseBtn->isChecked());
    }
}

void MainWindow::findPrev()
{
    EditorView *v = currentView();
    if (!v) {
        return;
    }
    if (v->hitCount() > 0) {
        v->prevHit();
    } else if (!m_searchEdit->text().isEmpty()) {
        m_findDebounce->stop();
        v->searchLive(m_searchEdit->text(), m_caseBtn->isChecked());
    }
}

// ---------------- vi-командная строка ----------------

void MainWindow::showCommandLine()
{
    m_cmdWrap->show();
    m_cmdLine->setFocus();
}

void MainWindow::hideCommandLine()
{
    m_cmdWrap->hide();
    if (EditorView *v = currentView()) {
        v->setFocus();
    }
}

void MainWindow::onCommandLine(const QString &text)
{
    EditorView *v = currentView();
    if (!v) {
        return;
    }
    const QString cmd = text.trimmed();
    if (cmd == "w") {
        v->save(false);
    } else if (cmd == "q") {
        onCloseTab(m_tabs->currentIndex());
    } else if (cmd == "q!") {
        if (v->docId() > 0) {
            m_bridge->closeDoc(v->docId());
        }
        m_tabs->removeTab(m_tabs->currentIndex());
        v->deleteLater();
    } else if (cmd == "wq") {
        v->save(false);
        onCloseTab(m_tabs->currentIndex());
    } else {
        bool ok = false;
        const qlonglong line = cmd.toLongLong(&ok);
        if (ok && line > 0) {
            statusBar()->showMessage(tr("Перехід до рядка %1…").arg(line), 2000);
            v->gotoLine(line);
        } else {
            statusBar()->showMessage(tr("Невідома команда: %1").arg(cmd), 4000);
        }
    }
}

bool MainWindow::eventFilter(QObject *obj, QEvent *e)
{
    // Esc в поиске/командной строке — скрыть и вернуть фокус редактору
    if (e->type() == QEvent::KeyPress) {
        auto *ke = static_cast<QKeyEvent *>(e);
        if (ke->key() == Qt::Key_Escape) {
            if (obj == m_cmdLine) {
                m_cmdLine->clear();
                hideCommandLine();
                return true;
            }
            if (m_searchBar && m_searchBar->isVisible()) {
                m_searchBar->hide();
                if (EditorView *v = currentView()) {
                    v->setFocus();
                }
                return true;
            }
        }
    }
    return QMainWindow::eventFilter(obj, e);
}

void MainWindow::showAbout()
{
    QMessageBox::about(
        this, tr("POLER Edit"),
        tr("<b>POLER Edit 2.0</b><br/>"
           "Суверенний текстовий редактор без лімітів розміру файлів.<br/><br/>"
           "Ядро: <b>poler-edit-core</b> (автономний Rust, Zero-Copy Piece-Table).<br/>"
           "Пошук: <b>Search Engine v2</b> — багатопотоковий SIMD-скан, "
           "живий потік збігів, ε-енергія значимості, резонанс R(t).<br/>"
           "Файли 1–10 ГБ відкриваються за 0.05 с; RAM не залежить від розміру.<br/><br/>"
           "Ліцензування Pro: Ed25519, офлайн-валідація.<br/>"
           "<a href=\"https://github.com/poler-engine-org/poler-edit-qt\">"
           "github.com/poler-engine-org/poler-edit-qt</a>"));
}

// ---------------- лицензия Pro (Ed25519, офлайн) ----------------

void MainWindow::showLicenseDialog()
{
    QDialog dlg(this);
    dlg.setWindowTitle(tr("Активація POLER Edit Pro"));
    dlg.setModal(true);
    auto *lay = new QVBoxLayout(&dlg);
    auto *info = new QLabel(
        tr("Введіть ліцензійний ключ видавця (формат <b>PE1-…</b>).<br/>"
           "Валідація — офлайн, підпис Ed25519 перевіряється локально в ядрі."),
        &dlg);
    info->setWordWrap(true);
    auto *edit = new QLineEdit(&dlg);
    edit->setPlaceholderText(QStringLiteral("PE1-..."));
    edit->setText(QSettings().value(QStringLiteral("license/key")).toString());
    edit->setMinimumWidth(420);
    auto *btns = new QDialogButtonBox(QDialogButtonBox::Ok | QDialogButtonBox::Cancel, &dlg);
    btns->button(QDialogButtonBox::Ok)->setText(tr("&Активувати"));
    connect(btns, &QDialogButtonBox::accepted, &dlg, &QDialog::accept);
    connect(btns, &QDialogButtonBox::rejected, &dlg, &QDialog::reject);
    lay->addWidget(info);
    lay->addWidget(edit);
    lay->addWidget(btns);

    if (dlg.exec() != QDialog::Accepted || edit->text().trimmed().isEmpty()) {
        return;
    }
    const QString key = edit->text().trimmed();
    m_bridge->licenseActivate(key, [this, key](const QJsonObject &r) {
        if (r.value("valid").toBool()) {
            QSettings().setValue(QStringLiteral("license/key"), key);
            applyLicenseLabel(r.value("name").toString(),
                              r.value("perpetual").toBool(),
                              r.value("expires").toVariant().toLongLong());
            QMessageBox::information(
                this, tr("Pro активовано"),
                tr("Дякуємо, %1! Ліцензію Pro активовано та збережено локально.")
                    .arg(r.value("name").toString()));
        } else {
            QMessageBox::warning(
                this, tr("Ключ не прийнято"),
                tr("Причина: %1").arg(r.value("reason").toString()));
        }
    });
}

void MainWindow::validateStoredLicense()
{
    const QString key = QSettings().value(QStringLiteral("license/key")).toString();
    if (key.isEmpty()) {
        return; // Free-режим — статус-бар уже показывает "Free"
    }
    m_bridge->licenseActivate(key, [this](const QJsonObject &r) {
        if (r.value("valid").toBool()) {
            applyLicenseLabel(r.value("name").toString(),
                              r.value("perpetual").toBool(),
                              r.value("expires").toVariant().toLongLong());
        } else {
            // Ключ просрочился/отозван — тихо возвращаемся в Free.
            QSettings().remove(QStringLiteral("license/key"));
        }
    });
}

void MainWindow::applyLicenseLabel(const QString &name, bool perpetual, qlonglong expires)
{
    if (!m_licLabel) {
        return;
    }
    QString until;
    if (perpetual) {
        until = tr("нескінченною");
    } else if (expires > 0) {
        until = QDateTime::fromSecsSinceEpoch(expires).date().toString(Qt::ISODate);
    }
    m_licLabel->setText(tr("Pro · %1").arg(name));
    m_licLabel->setStyleSheet("color: #f39c12; font-weight: 700;");
    m_licLabel->setToolTip(tr("POLER Edit Pro — %1 (до %2)")
                               .arg(name, until.isEmpty() ? tr("—") : until));
}
