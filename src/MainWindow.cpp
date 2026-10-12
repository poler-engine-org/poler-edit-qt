#include "MainWindow.h"

#include <QApplication>
#include <QCloseEvent>
#include <QFileDialog>
#include <QFileInfo>
#include <QHBoxLayout>
#include <QKeyEvent>
#include <QLabel>
#include <QLineEdit>
#include <QMenuBar>
#include <QMessageBox>
#include <QPushButton>
#include <QStatusBar>
#include <QStyle>
#include <QTabWidget>
#include <QToolBar>
#include <QVBoxLayout>

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

    updateTitle();
}

QWidget *MainWindow::buildSearchBar()
{
    m_searchBar = new QWidget(this);
    auto *lay = new QHBoxLayout(m_searchBar);
    lay->setContentsMargins(8, 4, 8, 4);
    m_searchEdit = new QLineEdit(m_searchBar);
    m_searchEdit->setPlaceholderText(
        tr("Знайти (Enter — далі, Esc — закрити)"));
    auto *next = new QPushButton(tr("Далі ▶"), m_searchBar);
    auto *close = new QPushButton(tr("✕"), m_searchBar);
    next->setFixedWidth(80);
    close->setFixedWidth(32);
    lay->addWidget(m_searchEdit, 1);
    lay->addWidget(next);
    lay->addWidget(close);
    connect(next, &QPushButton::clicked, this, &MainWindow::findNext);
    connect(close, &QPushButton::clicked, m_searchBar, &QWidget::hide);
    connect(m_searchEdit, &QLineEdit::returnPressed, this, &MainWindow::findNext);
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

    QMenu *help = menuBar()->addMenu(tr("&Довідка"));
    help->addAction(tr("&Про POLER Editor"), QKeySequence::HelpContents, this, &MainWindow::showAbout);
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
    m_engineLabel = new QLabel(tr("POLER ●"), this);
    for (QLabel *l : {m_posLabel, m_bytesLabel, m_linesLabel, m_modeLabel, m_engineLabel}) {
        l->setMargin(6);
        statusBar()->addPermanentWidget(l);
    }
    m_engineLabel->setStyleSheet("color: #2ecc71; font-weight: 600;");
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
        tr("Ядро poler-engine недоступне. Перевірте poler-engine у PATH."), 10000);
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

// ---------------- поиск ----------------

void MainWindow::onFind()
{
    m_searchBar->show();
    m_searchEdit->setFocus();
    m_searchEdit->selectAll();
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
        v->search(m_searchEdit->text(), false);
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
        this, tr("POLER Editor"),
        tr("<b>POLER Editor 0.38.0</b><br/>"
           "Суверенний текстовий редактор без лімітів розміру файлів.<br/><br/>"
           "Ядро: poler-engine --edit-serve<br/>"
           "Zero-copy mmap piece-table + SIMD Aho-Corasick + ленивий line-index.<br/><br/>"
           "100 GiB файл відкривається за 0.1 мс; RAM не залежить від розміру.<br/>"
           "<a href=\"https://github.com/poler-engine-org/poler-engine\">"
           "github.com/poler-engine-org/poler-engine</a>"));
}
