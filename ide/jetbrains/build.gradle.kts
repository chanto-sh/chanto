plugins {
    id("java")
    id("org.jetbrains.kotlin.jvm") version "2.2.0"
    id("org.jetbrains.intellij.platform") version "2.19.0"
}

group = "dev.chanto"
version = "0.1.11"

// IntelliJ IDEA Community stopped being published at 2025.3, so the default is
// the unified `idea` distribution. Point -PlocalIde=/path/to/IDE.app at an
// installed IDE to build against that instead of downloading one.
val localIde: String? = providers.gradleProperty("localIde").orNull

repositories {
    mavenCentral()
    intellijPlatform {
        defaultRepositories()
    }
}

dependencies {
    intellijPlatform {
        if (localIde != null) {
            local(localIde)
        } else {
            intellijIdea(providers.gradleProperty("platformVersion").get())
        }
        bundledPlugin("org.jetbrains.plugins.terminal")
        pluginVerifier()
        zipSigner()
    }
}

kotlin {
    jvmToolchain(21)
}

intellijPlatform {
    // The bridge binds a localhost port at startup; the settings index is not useful here.
    buildSearchableOptions = false

    pluginConfiguration {
        ideaVersion {
            sinceBuild = "253"
        }
    }

    pluginVerification {
        ides {
            recommended()
        }
    }
}
