// POLER Edit — точка входа.
// Kate-подобный суверенный редактор: GUI здесь тонкий, вся работа с текстом
// любого размера — в автономном Rust-ядре poler-edit-core (JSON-lines stdio).

#include <QApplication>
#include <QCommandLineParser>
#include <QFile>
#include <QPalette>
#include <QStyleFactory>

#include "MainWindow.h"

static void applyDarkPalette(QApplication &app)
{
    // Breeze-тёмная гамма (KDE Dark): фон #232629, панели #31363b,
    // текст #eff0f1, акцент #3daee9.
    app.setStyle(QStyleFactory::create("Fusion"));
    QPalette p;
    const QColor window(0x23, 0x26, 0x29);
    const QColor base(0x25, 0x28, 0x2c);
    const QColor button(0x31, 0x36, 0x3b);
    const QColor text(0xef, 0xf0, 0xf1);
    const QColor highlight(0x3d, 0xae, 0xe9);
    p.setColor(QPalette::Window, window);
    p.setColor(QPalette::WindowText, text);
    p.setColor(QPalette::Base, base);
    p.setColor(QPalette::AlternateBase, window);
    p.setColor(QPalette::Text, text);
    p.setColor(QPalette::Button, button);
    p.setColor(QPalette::ButtonText, text);
    p.setColor(QPalette::Highlight, highlight);
    p.setColor(QPalette::HighlightedText, Qt::black);
    p.setColor(QPalette::ToolTipBase, base);
    p.setColor(QPalette::ToolTipText, text);
    p.setColor(QPalette::PlaceholderText, QColor(0x7f, 0x8c, 0x8d));
    p.setColor(QPalette::Disabled, QPalette::Text, QColor(0x6d, 0x70, 0x74));
    p.setColor(QPalette::Disabled, QPalette::ButtonText, QColor(0x6d, 0x70, 0x74));
    app.setPalette(p);
}

int main(int argc, char *argv[])
{
    QApplication app(argc, argv);
    QApplication::setApplicationName("poler-edit");
    QApplication::setApplicationVersion("2.0.0");
    QApplication::setOrganizationName("POLER Engine");

    QCommandLineParser cli;
    cli.setApplicationDescription(
        "POLER Edit — суверенний текстовий редактор без лiмiтiв розмiру файлiв.\n"
        "Ядро: poler-edit-core (zero-copy mmap piece-table, Search Engine v2).\n"
        "Пошук: живий потiк збiгiв, ε-енергiя значимостi, резонанс R(t).");
    cli.addHelpOption();
    cli.addVersionOption();
    cli.addOption({{"l", "light"}, "светлая тема (по умолчанию тёмная, как Breeze Dark)"});
    cli.addPositionalArgument("files", "файлы для открытия", "[files...]");
    cli.process(app);

    if (!cli.isSet("light")) {
        applyDarkPalette(app);
    }

    MainWindow win;
    win.show();

    const QStringList args = cli.positionalArguments();
    if (args.isEmpty()) {
        win.openUntitled();
    } else {
        for (const QString &f : args) {
            win.openFile(f);
        }
    }
    return app.exec();
}
