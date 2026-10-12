// Асинхронный мост к `poler-engine --edit-serve` (JSON lines поверх stdio).
// Паттерн проверен интеграцией kate-poler: неблокирующий QProcess,
// построчная буферизация stdout, ответы маршрутизируются по reqId.

#pragma once

#include <QHash>
#include <QJsonObject>
#include <QProcess>
#include <QString>
#include <functional>

class EngineBridge : public QObject
{
    Q_OBJECT
public:
    using Callback = std::function<void(const QJsonObject &)>;

    explicit EngineBridge(QObject *parent = nullptr);
    ~EngineBridge() override;

    bool isRunning() const { return m_proc && m_proc->state() == QProcess::Running; }

    // Все команды асинхронны; ответ приходит в cb (в Gui-потоке).
    qlonglong open(const QString &path, const Callback &cb);
    qlonglong closeDoc(qlonglong doc);
    qlonglong viewport(qlonglong doc, qlonglong line, int count, const Callback &cb);
    qlonglong indexDoc(qlonglong doc, const Callback &cb);
    qlonglong insertAt(qlonglong doc, qlonglong line, qlonglong col,
                       const QString &text, const Callback &cb);
    qlonglong del(qlonglong doc, qlonglong sl, qlonglong sc,
                  qlonglong el, qlonglong ec, const Callback &cb);
    qlonglong search(qlonglong doc, const QString &query, bool caseSensitive,
                     int limit, const Callback &cb);
    qlonglong save(qlonglong doc, const Callback &cb);
    qlonglong saveAs(qlonglong doc, const QString &path, const Callback &cb);
    qlonglong undo(qlonglong doc, const Callback &cb);
    qlonglong redo(qlonglong doc, const Callback &cb);
    void cancel();
    void quit();

    static QString enginePath();

signals:
    // События сервера: {"ev":"progress","op":...,"doc":...}
    void progressEvent(const QJsonObject &obj);
    void engineDied();

private:
    void send(const QJsonObject &obj);
    qlonglong nextId() { return ++m_reqId; }
    void drainStdout();
    void ensureStarted();

    QProcess *m_proc = nullptr;
    QByteArray m_buf;
    qlonglong m_reqId = 0;
    QHash<qlonglong, Callback> m_pending;
};
