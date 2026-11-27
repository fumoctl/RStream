# Keep RStream's Kotlin JNI bridges loaded dynamically via ClassLoader.loadClass from Rust (src/android.rs).
-keep class dev.rstream.app.** {
  *;
}
# Keep rustls-platform-verifier's JNI-loaded Android certificate verifier.
-keep, includedescriptorclasses class org.rustls.platformverifier.** { *; }
