/// Expose the `Java_dev_dioxus_main_Rust_create` function to the JNI layer.
/// We hardcode these to have a single trampoline for host Java code to call into.
///
/// This saves us from having to plumb the top-level package name all the way down into
/// this file. This is better for modularity (ie just call dioxus' main to run the app) as
/// well as cache thrashing since this crate doesn't rely on external env vars.
///
/// The CLI is expecting to find `dev.dioxus.main` in the final library. If you find a need to
/// change this, you'll need to change the CLI as well.

/// Gate that `root()` waits on until `android_setup_with_ndk_context` has completed.
///
/// `WryActivity.kt` calls `addObserver` (which fires `Rust.create()` synchronously) *before*
/// `Rust.onActivityCreate()`. `Rust.create()` spawns our `root()` on a background thread, so
/// `root()` races with `onActivityCreate`. We use this condvar to make `root()` wait until
/// `android_setup_with_ndk_context` (called from `onActivityCreate`) has registered the
/// activity in wry's `ACTIVITY_PROXY` and initialized `ndk_context`. Without this gate the
/// app crashes with "no available activity" when the WebView is created.
#[cfg(target_os = "android")]
static ANDROID_SETUP_CONDVAR: std::sync::LazyLock<(std::sync::Mutex<bool>, std::sync::Condvar)> =
    std::sync::LazyLock::new(|| (std::sync::Mutex::new(false), std::sync::Condvar::new()));

/// Wrapper around `wry::android_setup` that also:
/// 1. Initializes `ndk_context` (tao 0.35.0 doesn't call `initialize_android_context`).
/// 2. Signals the condvar so that `root()` can proceed to call `main()`.
#[cfg(target_os = "android")]
unsafe fn android_setup_with_ndk_context(
    package: &str,
    env: jni::JNIEnv,
    looper: &ndk::looper::ThreadLooper,
    activity: jni::objects::GlobalRef,
) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static INITIALIZED: AtomicBool = AtomicBool::new(false);

    let vm = env.get_java_vm().unwrap();
    // On activity recreation release the old context first to satisfy
    // ndk_context's assert!(previous.is_none()) in initialize_android_context.
    if INITIALIZED.swap(true, Ordering::SeqCst) {
        ndk_context::release_android_context();
    }
    ndk_context::initialize_android_context(
        vm.get_java_vm_pointer() as *mut std::ffi::c_void,
        activity.as_obj().as_raw() as *mut std::ffi::c_void,
    );

    wry::android_setup(package, env, looper, activity);

    // Signal root() that it may proceed.
    let (lock, cvar) = &*ANDROID_SETUP_CONDVAR;
    let mut ready = lock.lock().unwrap();
    *ready = true;
    cvar.notify_all();
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn start_app() {
    use crate::Config;
    use dioxus_core::{Element, VirtualDom};
    use std::any::Any;

    tao::android_binding!(
        dev_dioxus,
        main,
        Rust,
        android_setup_with_ndk_context,
        root,
        tao
    );
    wry::android_binding!(dev_dioxus, main, wry);

    #[cfg(target_os = "android")]
    fn root() {
        // Wait until android_setup_with_ndk_context has completed (called from
        // Rust.onActivityCreate). WryActivity.kt fires Rust.create() (which spawns this
        // thread) *before* Rust.onActivityCreate(), so without this gate we would race
        // and crash with "no available activity" when wry tries to create the WebView.
        //
        // Deadlock safety:
        // - wait_while checks the predicate before parking, so a "missed signal" (setup
        //   already ran before we reach this point) is impossible.
        // - We use wait_timeout_while with a 10 s ceiling so that if Kotlin never calls
        //   Rust.onActivityCreate() (e.g. a crash/exception before onCreate reaches that
        //   line) we abort instead of hanging the process silently.
        {
            let (lock, cvar) = &*ANDROID_SETUP_CONDVAR;
            let (c, timed_out) = cvar
                .wait_timeout_while(
                    lock.lock().unwrap(),
                    std::time::Duration::from_secs(10),
                    |ready| !*ready,
                )
                .unwrap();
            if timed_out.timed_out() {
                panic!("android_setup_with_ndk_context was never called within 10s — onActivityCreate did not fire");
            }
        }

        fn stop_unwind<F: FnOnce() -> T, T>(f: F) -> T {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
                Ok(t) => t,
                Err(err) => {
                    eprintln!("attempt to unwind out of `rust` with err: {:?}", err);
                    std::process::abort()
                }
            }
        }

        stop_unwind(|| unsafe {
            let mut main_fn_ptr = libc::dlsym(libc::RTLD_DEFAULT, b"main\0".as_ptr() as _);

            if main_fn_ptr.is_null() {
                main_fn_ptr = libc::dlsym(libc::RTLD_DEFAULT, b"_main\0".as_ptr() as _);
            }

            if main_fn_ptr.is_null() {
                panic!("Failed to find main symbol");
            }

            // Set the env vars that rust code might expect, passed off to us by the android app
            // Doing this before main emulates the behavior of a regular executable
            if cfg!(target_os = "android") && cfg!(debug_assertions) {
                // Load the env file from the session cache if we're in debug mode and on android
                //
                // This is a slightly hacky way of being able to use std::env::var code in android apps without
                // going through their custom java-based system.
                let env_file = dioxus_cli_config::android_session_cache_dir().join(".env");
                if let Ok(env_file) = std::fs::read_to_string(&env_file) {
                    for line in env_file.lines() {
                        if let Some((key, value)) = line.trim().split_once('=') {
                            std::env::set_var(key, value);
                        }
                    }
                }
            }

            let main_fn: extern "C" fn() = std::mem::transmute(main_fn_ptr);
            main_fn();
        });
    }
}
