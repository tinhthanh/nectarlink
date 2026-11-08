// SPDX-License-Identifier: GPL-3.0-or-later
//
// Vetgo Phone: a small Android app for vglink (a Shipyard plugin) on
// NectarLink's core. It only shares texts, calls, contacts and the
// notifications of chat apps with the paired box, and is set up over the
// USB cable by the box (installed with `adb install -g`, granted the rest
// with adb), so nothing is tapped on the phone.
//
// The Rust core and its Kotlin bindings are built as for :app. The few
// classes it shares with :app (texts, calls, contacts, reading
// notifications, the call's sound) are copied from there at build time, so
// both apps stay on one copy of that code.
import javax.inject.Inject

plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.compose)
}

android {
    namespace = "vn.vetgo.phone"
    compileSdk = 37
    ndkVersion = "30.0.16248370"

    defaultConfig {
        applicationId = "vn.vetgo.phone"
        minSdk = 26
        targetSdk = 37
        versionCode = 4
        versionName = "0.1.3"
    }

    buildTypes {
        release {
            // Signed with the debug key until the app has its own (local builds).
            signingConfig = signingConfigs.getByName("debug")
            // R8 drops the unused code (Compose, coroutines, ...); the core's
            // library is built with the android-small profile.
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    buildFeatures {
        compose = true
        buildConfig = true
    }

    // The core's library compressed in the APK: the box fetches and installs
    // it over the cable, so the download is what counts (about a third the
    // size), not the few MB the phone unpacks it into.
    packaging {
        jniLibs {
            useLegacyPackaging = true
        }
    }
}

/** Builds the Rust core into `<outputDir>/<abi>/libnectarlink_ffi.so`. */
abstract class PhoneCargoNdkBuild : DefaultTask() {
    @get:Inject abstract val exec: ExecOperations
    @get:Input abstract val abis: ListProperty<String>
    @get:Input abstract val profile: Property<String>
    @get:InputDirectory @get:PathSensitive(PathSensitivity.RELATIVE) abstract val crates: DirectoryProperty
    @get:InputFile @get:PathSensitive(PathSensitivity.NONE) abstract val lockFile: RegularFileProperty
    /** The workspace manifest: its profiles decide how the library is built. */
    @get:InputFile @get:PathSensitive(PathSensitivity.NONE) abstract val manifest: RegularFileProperty
    @get:Internal abstract val workspace: DirectoryProperty
    @get:Internal abstract val ndk: DirectoryProperty
    @get:OutputDirectory abstract val outputDir: DirectoryProperty

    @TaskAction
    fun build() {
        exec.exec {
            workingDir = workspace.get().asFile
            environment("ANDROID_NDK_HOME", ndk.get().asFile.absolutePath)
            commandLine(
                buildList {
                    addAll(listOf("cargo", "ndk", "--platform", "26", "-o", outputDir.get().asFile.path))
                    abis.get().forEach { addAll(listOf("-t", it)) }
                    addAll(listOf("build", "--profile", profile.get(), "--locked", "-p", "nectarlink-ffi"))
                },
            )
        }
    }
}

/** Generates the Kotlin bindings from a built core library. */
abstract class PhoneUniffiBindgen : DefaultTask() {
    @get:Inject abstract val exec: ExecOperations
    @get:InputFile @get:PathSensitive(PathSensitivity.NONE) abstract val library: RegularFileProperty
    @get:Internal abstract val workspace: DirectoryProperty
    @get:OutputDirectory abstract val outputDir: DirectoryProperty

    @TaskAction
    fun generate() {
        exec.exec {
            workingDir = workspace.get().asFile
            commandLine(
                "cargo", "run", "--quiet", "--locked", "-p", "nectarlink-ffi", "--features", "bindgen",
                "--bin", "uniffi-bindgen", "--", "generate", "--library", library.get().asFile.path,
                "--language", "kotlin", "--no-format", "--out-dir", outputDir.get().asFile.path,
            )
        }
    }
}

/** Copies the classes shared with :app into `<outputDir>`. */
abstract class SharedSources : DefaultTask() {
    @get:Inject abstract val fs: FileSystemOperations
    @get:InputDirectory @get:PathSensitive(PathSensitivity.RELATIVE) abstract val from: DirectoryProperty
    @get:Input abstract val files: ListProperty<String>
    @get:OutputDirectory abstract val outputDir: DirectoryProperty

    @TaskAction
    fun copy() {
        fs.sync {
            from(from) { files.get().forEach { include(it) } }
            into(outputDir)
        }
    }
}

val workspaceDir: Directory = rootProject.layout.projectDirectory.dir("..")
val rustAbis: List<String> =
    (findProperty("nectarlink.abis") as String?)?.split(",")?.map(String::trim) ?: listOf("arm64-v8a", "x86_64")

/** What this app takes from :app, unchanged. */
val shared = listOf(
    "app/nectarlink/android/sms/PhoneSms.kt",
    "app/nectarlink/android/calls/PhoneCalls.kt",
    "app/nectarlink/android/calls/CallCompanion.kt",
    "app/nectarlink/android/contacts/PhoneContacts.kt",
    "app/nectarlink/android/notifications/NotificationReader.kt",
    "app/nectarlink/android/notifications/LiveRateLimiter.kt",
    "app/nectarlink/android/core/KeystoreKeyProtector.kt",
    "app/nectarlink/android/elevated/CallAudio.kt",
)

androidComponents {
    onVariants { variant ->
        val suffix = variant.name.replaceFirstChar(Char::uppercase)
        val cargo = tasks.register<PhoneCargoNdkBuild>("cargoNdkBuild$suffix") {
            description = "Builds the Rust core for Android."
            abis.set(rustAbis)
            profile.set(if (variant.buildType == "release") "android-small" else "android")
            crates.set(workspaceDir.dir("core"))
            lockFile.set(workspaceDir.file("Cargo.lock"))
            manifest.set(workspaceDir.file("Cargo.toml"))
            workspace.set(workspaceDir)
            ndk.set(androidComponents.sdkComponents.ndkDirectory)
        }
        val bindgen = tasks.register<PhoneUniffiBindgen>("uniffiBindgen$suffix") {
            description = "Generates the Kotlin bindings for the Rust core."
            library.set(cargo.flatMap { it.outputDir.file("${rustAbis.first()}/libnectarlink_ffi.so") })
            workspace.set(workspaceDir)
        }
        val sharedSources = tasks.register<SharedSources>("sharedSources$suffix") {
            description = "Copies the classes shared with the NectarLink app."
            from.set(rootProject.layout.projectDirectory.dir("app/src/main/kotlin"))
            files.set(shared)
        }
        variant.sources.jniLibs?.addGeneratedSourceDirectory(cargo, PhoneCargoNdkBuild::outputDir)
        variant.sources.java?.addGeneratedSourceDirectory(bindgen, PhoneUniffiBindgen::outputDir)
        variant.sources.java?.addGeneratedSourceDirectory(sharedSources, SharedSources::outputDir)
    }
}

dependencies {
    implementation(platform(libs.compose.bom))
    implementation(libs.compose.ui)
    implementation(libs.compose.material3)
    implementation(libs.androidx.core.ktx)
    implementation(libs.androidx.activity.compose)
    implementation(libs.androidx.lifecycle.runtime.compose)
    implementation(libs.kotlinx.coroutines.android)
    // UniFFI's Kotlin bindings call the core through JNA.
    implementation(libs.jna) { artifact { type = "aar" } }
}
