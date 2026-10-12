// Главное окно POLER Editor: Kate-подобный интерфейс.
// Меню, панель инструментов, вкладки документов, статусная строка,
// vi-подобная командная строка (:w :q :wq :123), панель поиска.

#pragma once

#include <QMainWindow>

#include "EditorView.h"

class QTabWidget;
class QLabel;
class QLineEdit;
class QPushButton;
class EngineBridge;

class MainWindow : public QMainWindow
{
    Q_OBJECT
public:
    explicit MainWindow(QWidget *parent = nullptr);

    void openFile(const QString &path);
    void openUntitled();

protected:
    void closeEvent(QCloseEvent *e) override;
    bool eventFilter(QObject *obj, QEvent *e) override;

private slots:
    void onOpen();
    void onSave();
    void onSaveAs();
    void onCloseTab(int index);
    void onFind();
    void findNext();
    void onCursorMoved(qlonglong line, qlonglong col);
    void onStatusMessage(const QString &msg, int timeoutMs);
    void onDocModified(EditorView *view, bool modified);
    void onProgress(const QJsonObject &obj);
    void onCommandLine(const QString &text);
    void showCommandLine();
    void hideCommandLine();
    void onEngineDied();
    void onTabChanged(int index);
    void showAbout();

private:
    EditorView *currentView() const;
    EditorView *viewAt(int index) const;
    void attachView(EditorView *view);
    QWidget *buildSearchBar();
    QWidget *buildCommandLine();
    void buildMenus();
    void buildToolbar();
    void buildStatusBar();
    void updateTabTitle(EditorView *view);
    void updateTitle();

    EngineBridge *m_bridge;
    QTabWidget *m_tabs = nullptr;
    QLineEdit *m_searchEdit = nullptr;
    QWidget *m_searchBar = nullptr;
    QLineEdit *m_cmdLine = nullptr;
    QWidget *m_cmdWrap = nullptr;

    QLabel *m_posLabel = nullptr;
    QLabel *m_bytesLabel = nullptr;
    QLabel *m_linesLabel = nullptr;
    QLabel *m_modeLabel = nullptr;
    QLabel *m_engineLabel = nullptr;
};
