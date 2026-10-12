// Главное окно POLER Editor: Kate-подобный интерфейс.
// Меню, панель инструментов, вкладки документов, статусная строка,
// vi-подобная командная строка (:w :q :wq :123), Live-панель поиска
// (Search Engine v2) и активация Pro-лицензии (Ed25519, офлайн).

#pragma once

#include <QMainWindow>

#include "EditorView.h"

class QTabWidget;
class QLabel;
class QLineEdit;
class QPushButton;
class QTimer;
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
    void findPrev();
    /// Debounce as-you-type: запуск live-поиска в активном документе.
    void runLiveSearch();
    /// (всего, текущий, скан идёт, лимит) — счётчик совпадений в панели.
    void onHitCountChanged(int total, int current, bool scanning, bool truncated);
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
    void showLicenseDialog();
    void validateStoredLicense();

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
    /// Статус-бар: "Pro · Имя" (до даты / безсрочно).
    void applyLicenseLabel(const QString &name, bool perpetual, qlonglong expires);

    EngineBridge *m_bridge;
    QTabWidget *m_tabs = nullptr;
    QLineEdit *m_searchEdit = nullptr;
    QWidget *m_searchBar = nullptr;
    QPushButton *m_caseBtn = nullptr;   // "Aa" — учитывать регистр
    QLabel *m_hitLabel = nullptr;       // счётчик совпадений live-поиска
    QTimer *m_findDebounce = nullptr;   // as-you-type debounce (мс)
    QLineEdit *m_cmdLine = nullptr;
    QWidget *m_cmdWrap = nullptr;

    QLabel *m_posLabel = nullptr;
    QLabel *m_bytesLabel = nullptr;
    QLabel *m_linesLabel = nullptr;
    QLabel *m_modeLabel = nullptr;
    QLabel *m_engineLabel = nullptr;
    QLabel *m_licLabel = nullptr;       // Free / Pro — статус лицензии
};
