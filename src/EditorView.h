// Редакторский вьюпорт: рендер ТОЛЬКО видимых строк (virtual viewport).
// Файл любого размера живёт в ядре poler-edit-core; здесь — кэш видимого окна
// с оверсканом и асинхронная подгрузка (поколения отбрасывают устаревшие).
//
// Search Engine v2: live-поиск as-you-type (search_live с приоритетным окном
// вьюпорта), подсветка совпадений на экране, F3/Shift+F3 навигация,
// счётчик совпадений и ε/R-метрики в статусе.

#pragma once

#include <QAbstractScrollArea>
#include <QStringList>
#include <QTimer>

#include "EngineBridge.h"

struct PolarHit
{
    qlonglong line = 0;
    /// Столбец в СИМВОЛАХ (v2: ядро отдаёт символьные колонки — раньше
    /// байтовые путались с символьными позициями курсора, кириллица прыгала).
    qlonglong col = 0;
    /// Длина совпадения в БАЙТАХ.
    qlonglong len = 0;
    /// Длина совпадения в СИМВОЛАХ (для подсветки).
    qlonglong lenChars = 0;
    qlonglong byte = 0;
    /// ε — энергия значимости совпадения (локальный порт poler-engine).
    double eps = 0.0;
};

class EditorView : public QAbstractScrollArea
{
    Q_OBJECT
public:
    EditorView(EngineBridge *bridge, const QString &path, QWidget *parent = nullptr);

    qlonglong docId() const { return m_doc; }
    QString filePath() const { return m_path; }
    QString fileName() const;
    bool isModified() const { return m_modified; }
    bool isIndexed() const { return m_linesTotal > 0; }

    // Search Engine v2: живой поиск (события hits приходят потоком;
    // первое окно вьюпорта — за миллисекунды).
    void searchLive(const QString &query, bool caseSensitive);
    void clearSearch();
    void jumpToHit(int index);
    void nextHit();
    void prevHit();
    void gotoLine(qlonglong line);
    int hitCount() const { return m_hits.size(); }
    bool searchActive() const { return !m_lastLiveQuery.isEmpty(); }
    void save(bool as);

signals:
    void docOpened(EditorView *view);
    void docModified(EditorView *view, bool modified);
    void cursorMoved(qlonglong line, qlonglong col);
    void statusMessage(const QString &msg, int timeoutMs);
    void requestClose(EditorView *view);
    void commandLineRequested(); // ':' — показать vi-командную строку
    /// Search v2: (всего найдено, индекс текущего, скан ещё идёт, обрезано лимитом).
    void hitCountChanged(int total, int current, bool scanning, bool truncated);

public slots:
    void undo();
    void redo();
    void selectAll();
    void copy();
    void cut();
    void paste();
    void zoomIn();
    void zoomOut();

protected:
    void paintEvent(QPaintEvent *e) override;
    void keyPressEvent(QKeyEvent *e) override;
    void mousePressEvent(QMouseEvent *e) override;
    void mouseMoveEvent(QMouseEvent *e) override;
    void mouseReleaseEvent(QMouseEvent *e) override;
    void mouseDoubleClickEvent(QMouseEvent *e) override;
    void wheelEvent(QWheelEvent *e) override;
    void resizeEvent(QResizeEvent *e) override;
    void focusInEvent(QFocusEvent *e) override;
    bool event(QEvent *e) override; // Tab focus + input method

private slots:
    void blinkCursor();
    void onProgress(const QJsonObject &obj);
    void onSearchEvent(const QJsonObject &obj);
    void reRunSearchAfterEdit();

private:
    // геометрия
    int lineHeight() const;
    int charWidth() const;
    int gutterWidth() const;
    int visibleLines() const;
    qlonglong topLine() const;
    void setTopLine(qlonglong line);

    // кэш видимого окна
    QString lineText(qlonglong line) const;
    qlonglong lineLen(qlonglong line) const;
    void requestFetch(qlonglong aroundLine);
    void fetchRange(qlonglong top, int count);

    // курсор/выделение
    struct Pos
    {
        qlonglong line = 0;
        qlonglong col = 0;
        bool operator<(const Pos &o) const
        {
            return line < o.line || (line == o.line && col < o.col);
        }
        bool operator==(const Pos &o) const { return line == o.line && col == o.col; }
    };
    Pos clampPos(const Pos &p) const;
    void setCursor(const Pos &p, bool keepAnchor = false);
    void ensureCursorVisible();
    bool hasSelection() const { return !(m_anchor == m_cursor); }
    QString selectedText() const;
    void deleteSelection();
    void insertText(const QString &text);
    void afterEdit();

    void updateScrollbars();
    void openDoc();
    void refreshInfo();

    // Search Engine v2
    void sortHitsIfDirty();
    void applyHitsArray(const QJsonArray &arr);
    void adoptSession(qlonglong sid); // снимает fence новой сессии (ack/событие)

    EngineBridge *m_bridge;
    QString m_path;
    qlonglong m_doc = 0;
    bool m_modified = false;
    bool m_indexing = false;
    qlonglong m_bytesTotal = 0;
    qlonglong m_linesTotal = 0; // 0 = неизвестно (индексация идёт)

    QStringList m_lines;   // кэш окна
    qlonglong m_blockTop = 0;
    qlonglong m_fetchGen = 0;   // отбрасывает устаревшие ответы
    qlonglong m_appliedGen = 0;

    Pos m_cursor;
    Pos m_anchor;
    bool m_cursorVisible = true;
    QTimer m_blink;

    QFont m_font;
    int m_zoom = 0;
    int m_baseSize = 10;

    // Search Engine v2: поток совпадений живого поиска.
    QList<PolarHit> m_hits;
    int m_hitIndex = 0;
    qlonglong m_curHitByte = -1; // якорь текущего совпадения при пересортировке
    bool m_hitsDirty = false;    // не отсортированы после инкрементального добавления
    // Fence против гонки ядро→GUI: поток поиска стартует ДО отправки ack,
    // событие "hits" может прийти раньше ack. Схема:
    //   searchLive() -> m_searchFence=true (все sid <= m_searchSid устарели);
    //   первое событие/ack с sid > m_searchSid снимает fence и сбрасывает
    //   выдачу; всё с sid < m_searchSid отбрасывается навсегда.
    bool m_searchActive = false; // есть текущая сессия (после clearSearch — false)
    bool m_searchFence = false;  // ждём sid новой сессии (ack ИЛИ событие)
    qlonglong m_searchSid = 0;   // sid сессии ядра (фильтр событий)
    bool m_searchDone = false;
    bool m_searchTrunc = false;
    bool m_searchJumped = false; // авто-переход к первому viewport-совпадению
    double m_searchR = 0.0;      // финальный резонанс сессии
    double m_searchGbps = 0.0;
    QString m_lastLiveQuery;
    bool m_searchCaseSensitive = true;
    QTimer m_research;           // повтор поиска после правок (debounce 400 мс)
};
