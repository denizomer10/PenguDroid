/*
 * Stand-in for an app's bundled C++ runtime (`libc++_shared.so`). It exports one
 * symbol that a sibling library depends on, so we can verify that PenguDroid
 * loads an APK's libraries as a set and resolves dependencies *within* the APK.
 */
int demo_cpp_answer(void) {
    return 42;
}
