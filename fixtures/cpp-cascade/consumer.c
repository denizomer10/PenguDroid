/*
 * A library that `DT_NEEDED`s the sibling `libc++_shared.so` and calls into it.
 * Loading succeeds only if PenguDroid stages the APK's libraries together.
 */
extern int demo_cpp_answer(void);

int consumer_run(void) {
    return demo_cpp_answer();
}
