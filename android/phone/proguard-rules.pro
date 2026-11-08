# Vetgo Phone: R8 shrinks, names stay readable (stack traces need no mapping).
-dontobfuscate
-keepattributes *Annotation*,Signature,InnerClasses,EnclosingMethod,SourceFile,LineNumberTable

# UniFFI + JNA look classes, fields and callbacks up by name at runtime
# (same rules as :app).
-keep class com.sun.jna.** { *; }
-keep interface com.sun.jna.** { *; }
-keep class * extends com.sun.jna.Structure {
    public <init>(...);
    <fields>;
    <methods>;
}
-keep class * implements com.sun.jna.Structure$ByValue { *; }
-keep class * implements com.sun.jna.Structure$ByReference { *; }
-keep class * implements com.sun.jna.Callback { *; }
-keep class * implements com.sun.jna.Library { *; }
-keepclassmembers class java.nio.Buffer {
    long address;
    int capacity;
}
-dontwarn com.sun.jna.**
-dontwarn java.awt.**
-keep class uniffi.** { *; }
-keep interface uniffi.** { *; }
-keep class app.nectarlink.core.** { *; }
-keep interface app.nectarlink.core.** { *; }

# The box starts the call's sound helper by name with app_process (vglink).
-keep class app.nectarlink.android.elevated.CallAudio { public static void main(java.lang.String[]); }
