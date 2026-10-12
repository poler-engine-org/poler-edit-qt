// Редакторский вьюпорт: рендер ТОЛЬКО видимых строк (virtual viewport).
// Файл любого размера живёт в ядре poler-edit; здесь — кэш видимого окна
// с оверсканом и асинхронная подгрузка (поколения отбрасывают устаревшие).

#pragma once

#include <QAbstractScrollArea>
#include <QStringList>
#include <QTimer>

#include "EngineBridge.h"

struct PolarHit
{
    qlonglong line = 0;
    qlonglong col = 0;
    qlonglong len = 0;
    qlonglong byte = 0;
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

    void search(const QString &query, bool caseSensitive);
    void jumpToHit(int index);
    void nextHit();
    void gotoLine(qlonglong line);
    int hitCount() const { return m_hits.size(); }
    void save(bool as);

signals:
    void docOpened(EditorView *view);
    void docModified(EditorView *view, bool modified);
    void cursorMoved(qlonglong line, qlonglong col);
    void statusMessage(const QString &msg, int timeoutMs);
    void requestClose(EditorView *view);
    void commandLineRequested(); // ':' — показать vi-командную строку

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

    QList<PolarHit> m_hits;
    int m_hitIndex = 0;
    QString m_lastQuery;
};
