cmake_minimum_required(VERSION 3.21)
project(poler-edit-qt LANGUAGES CXX)

# POLER Editor — Kate-подобный GUI поверх суверенного ядра poler-edit.
# Зависимости: ТОЛЬКО Qt6 Widgets (без KF6/KParts — тонкий клиент
# к `poler-engine --edit-serve` через JSON-протокол, LSP-стиль).

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_AUTOMOC ON)

find_package(Qt6 REQUIRED COMPONENTS Widgets)

add_executable(poler-edit
    src/main.cpp
    src/EngineBridge.cpp
    src/EngineBridge.h
    src/EditorView.cpp
    src/EditorView.h
    src/MainWindow.cpp
    src/MainWindow.h
)

target_link_libraries(poler-edit PRIVATE Qt6::Widgets)

include(GNUInstallDirs)
install(TARGETS poler-edit RUNTIME DESTINATION ${CMAKE_INSTALL_BINDIR})
