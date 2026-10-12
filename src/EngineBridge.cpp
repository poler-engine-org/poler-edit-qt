#include "EngineBridge.h"

#include <QCoreApplication>
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
    const QByteArray env = qgetenv("POLER_EDIT_CORE");
    if (!env.isEmpty() && QFile::exists(QString::fromLocal8Bit(env))) {
        return QString::fromLocal8Bit(env);
    }
    // 2) Рядом с исполняемым файлом редактора (портативный режим, Windows / macOS / Linux)
    const QString appDir = QCoreApplication::applicationDirPath();
    for (const QString &appCand : {appDir + "/poler-edit-core",
                                  appDir + "/poler-edit-core.exe",
                                  appDir + "/../bin/poler-edit-core",
                                  appDir + "/../core/target/release/poler-edit-core"}) {
        if (QFile::exists(appCand)) {
            return appCand;
        }
    }
    // 3) Пользовательский бинарник (~/.local/bin/poler-edit-core)
    const QString localUserPath = QDir::homePath() + "/.local/bin/poler-edit-core";
    if (QFile::exists(localUserPath)) {
        return localUserPath;
    }
    // 4) PATH
    const QString inPath = QStandardPaths::findExecutable("poler-edit-core");
    if (!inPath.isEmpty()) {
        return inPath;
    }
    // 5) Резервный системный путь
    for (const QString &cand : {QString("/usr/local/bin/poler-edit-core"),
                                QString("/usr/bin/poler-edit-core")}) {
        if (QFile::exists(cand)) {
            return cand;
        }
    }
    return QStringLiteral("poler-edit-core");
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
    const QString bin = enginePath();
    QStringList args;
    if (bin.contains(QLatin1String("poler-engine"))) {
        args << QStringLiteral("--edit-serve");
    }
    m_proc->start(bin, args);
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
        const QString ev = obj.value("ev").toString();
        if (ev == QLatin1String("progress")) {
            emit progressEvent(obj);
            continue;
        }
        if (!ev.isEmpty()) {
            // Search Engine v2: hits / viewport_done / search_done.
            emit searchEvent(obj);
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

qlonglong EngineBridge::searchLive(qlonglong doc, const QString &query, bool caseSensitive,
                                    int limit, qlonglong vpFrom, qlonglong vpTo,
                                    const Callback &cb)
{
    const qlonglong id = nextId();
    m_pending.insert(id, cb);
    QJsonArray vp;
    vp.append(double(vpFrom));
    vp.append(double(vpTo));
    send(QJsonObject{{"id", double(id)}, {"cmd", "search_live"}, {"doc", double(doc)},
                     {"query", query}, {"case_sensitive", caseSensitive},
                     {"limit", limit}, {"viewport", vp}});
    return id;
}

qlonglong EngineBridge::licenseActivate(const QString &key, const Callback &cb)
{
    const qlonglong id = nextId();
    m_pending.insert(id, cb);
    send(QJsonObject{{"id", double(id)}, {"cmd", "license_activate"}, {"key", key}});
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
