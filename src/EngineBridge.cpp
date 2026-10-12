#include "EngineBridge.h"

#include <QDir>
#include <QFile>
#include <QJsonDocument>
#include <QJsonArray>
#include <QStandardPaths>

EngineBridge::EngineBridge(QObject *parent)
    : QObject(parent)
{
}

EngineBridge::~EngineBridge()
{
    quit();
}

QString EngineBridge::enginePath()
{
    // 1) Явное переопределение окружением.
    const QByteArray env = qgetenv("POLER_ENGINE");
    if (!env.isEmpty() && QFile::exists(QString::fromLocal8Bit(env))) {
        return QString::fromLocal8Bit(env);
    }
    // 2) Суверенный пользовательский бинарник (~/.local/bin/poler-engine)
    const QString localUserPath = QDir::homePath() + "/.local/bin/poler-engine";
    if (QFile::exists(localUserPath)) {
        return localUserPath;
    }
    // 3) PATH
    const QString inPath = QStandardPaths::findExecutable("poler-engine");
    if (!inPath.isEmpty()) {
        return inPath;
    }
    // 3) Типичные суверенные пути.
    for (const QString &cand : {QDir::homePath() + "/.local/bin/poler-engine",
                                QString("/usr/local/bin/poler-engine"),
                                QString("/usr/bin/poler-engine")}) {
        if (QFile::exists(cand)) {
            return cand;
        }
    }
    return QString("poler-engine");
}

void EngineBridge::ensureStarted()
{
    if (m_proc && m_proc->state() != QProcess::NotRunning) {
        return;
    }
    if (!m_proc) {
        m_proc = new QProcess(this);
        m_proc->setProcessChannelMode(QProcess::SeparateChannels);
        connect(m_proc, &QProcess::readyReadStandardOutput, this, &EngineBridge::drainStdout);
        connect(m_proc, &QProcess::errorOccurred, this, [this] { emit engineDied(); });
        connect(m_proc, &QProcess::finished, this, [this] { emit engineDied(); });
    }
    m_proc->start(enginePath(), {QStringLiteral("--edit-serve")});
}

void EngineBridge::send(const QJsonObject &obj)
{
    ensureStarted();
    if (!m_proc || m_proc->state() == QProcess::NotRunning) {
        return;
    }
    const QByteArray line = QJsonDocument(obj).toJson(QJsonDocument::Compact);
    m_proc->write(line);
    m_proc->write("\n");
}

void EngineBridge::drainStdout()
{
    if (!m_proc) {
        return;
    }
    m_buf += m_proc->readAllStandardOutput();
    int nl;
    while ((nl = m_buf.indexOf('\n')) >= 0) {
        const QByteArray line = m_buf.left(nl);
        m_buf.remove(0, nl + 1);
        if (line.trimmed().isEmpty()) {
            continue;
        }
        const QJsonDocument doc = QJsonDocument::fromJson(line);
        if (!doc.isObject()) {
            continue;
        }
        const QJsonObject obj = doc.object();
        if (obj.contains("ev")) {
            emit progressEvent(obj);
            continue;
        }
        const qlonglong id = obj.value("id").toVariant().toLongLong();
        const auto it = m_pending.find(id);
        if (it != m_pending.end()) {
            Callback cb = it.value();
            m_pending.erase(it);
            if (cb) {
                cb(obj);
            }
        }
    }
}

qlonglong EngineBridge::open(const QString &path, const Callback &cb)
{
    const qlonglong id = nextId();
    m_pending.insert(id, cb);
    send(QJsonObject{{"id", double(id)}, {"cmd", "open"}, {"path", path}});
    return id;
}

qlonglong EngineBridge::closeDoc(qlonglong doc)
{
    const qlonglong id = nextId();
    send(QJsonObject{{"id", double(id)}, {"cmd", "close"}, {"doc", double(doc)}});
    return id;
}

qlonglong EngineBridge::viewport(qlonglong doc, qlonglong line, int count, const Callback &cb)
{
    const qlonglong id = nextId();
    m_pending.insert(id, cb);
    send(QJsonObject{{"id", double(id)}, {"cmd", "viewport"},
                     {"doc", double(doc)}, {"line", double(line)}, {"count", count}});
    return id;
}

qlonglong EngineBridge::indexDoc(qlonglong doc, const Callback &cb)
{
    const qlonglong id = nextId();
    m_pending.insert(id, cb);
    send(QJsonObject{{"id", double(id)}, {"cmd", "index"}, {"doc", double(doc)}});
    return id;
}

qlonglong EngineBridge::insertAt(qlonglong doc, qlonglong line, qlonglong col,
                                 const QString &text, const Callback &cb)
{
    const qlonglong id = nextId();
    m_pending.insert(id, cb);
    send(QJsonObject{{"id", double(id)}, {"cmd", "insert_at"}, {"doc", double(doc)},
                     {"line", double(line)}, {"col", double(col)}, {"text", text}});
    return id;
}

qlonglong EngineBridge::del(qlonglong doc, qlonglong sl, qlonglong sc,
                            qlonglong el, qlonglong ec, const Callback &cb)
{
    const qlonglong id = nextId();
    m_pending.insert(id, cb);
    send(QJsonObject{{"id", double(id)}, {"cmd", "delete"}, {"doc", double(doc)},
                     {"start_line", double(sl)}, {"start_col", double(sc)},
                     {"end_line", double(el)}, {"end_col", double(ec)}});
    return id;
}

qlonglong EngineBridge::search(qlonglong doc, const QString &query, bool caseSensitive,
                               int limit, const Callback &cb)
{
    const qlonglong id = nextId();
    m_pending.insert(id, cb);
    send(QJsonObject{{"id", double(id)}, {"cmd", "search"}, {"doc", double(doc)},
                     {"query", query}, {"case_sensitive", caseSensitive},
                     {"limit", limit}});
    return id;
}

qlonglong EngineBridge::save(qlonglong doc, const Callback &cb)
{
    const qlonglong id = nextId();
    m_pending.insert(id, cb);
    send(QJsonObject{{"id", double(id)}, {"cmd", "save"}, {"doc", double(doc)}});
    return id;
}

qlonglong EngineBridge::saveAs(qlonglong doc, const QString &path, const Callback &cb)
{
    const qlonglong id = nextId();
    m_pending.insert(id, cb);
    send(QJsonObject{{"id", double(id)}, {"cmd", "save_as"},
                     {"doc", double(doc)}, {"path", path}});
    return id;
}

qlonglong EngineBridge::undo(qlonglong doc, const Callback &cb)
{
    const qlonglong id = nextId();
    m_pending.insert(id, cb);
    send(QJsonObject{{"id", double(id)}, {"cmd", "undo"}, {"doc", double(doc)}});
    return id;
}

qlonglong EngineBridge::redo(qlonglong doc, const Callback &cb)
{
    const qlonglong id = nextId();
    m_pending.insert(id, cb);
    send(QJsonObject{{"id", double(id)}, {"cmd", "redo"}, {"doc", double(doc)}});
    return id;
}

void EngineBridge::cancel()
{
    send(QJsonObject{{"id", double(nextId())}, {"cmd", "cancel"}});
}

void EngineBridge::quit()
{
    if (m_proc && m_proc->state() != QProcess::NotRunning) {
        send(QJsonObject{{"id", double(nextId())}, {"cmd", "quit"}});
        m_proc->waitForFinished(500);
        m_proc->kill();
    }
}
