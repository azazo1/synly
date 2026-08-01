import groovy.json.JsonSlurper

plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.android)
    alias(libs.plugins.kotlin.compose)
}

val synlyVersion: String = runCatching {
    val metadata = providers.exec {
        commandLine("cargo", "metadata", "--locked", "--no-deps", "--format-version", "1")
    }.standardOutput.asText.get()
    val root = JsonSlurper().parseText(metadata) as Map<*, *>
    val packages = root["packages"] as List<*>
    val pkg = packages.first { (it as Map<*, *>)["name"] == "synly" } as Map<*, *>
    pkg["version"] as String
}.getOrElse { error ->
    throw GradleException("无法读取 synly 版本, 请确认已安装 Rust 工具链且 cargo 在 PATH 中", error)
}

android {
    namespace = "com.azazo1.synly"
    compileSdk = 36

    defaultConfig {
        applicationId = "com.azazo1.synly"
        minSdk = 29
        targetSdk = 36
        versionCode = 1
        versionName = synlyVersion
        ndk {
            abiFilters += listOf("arm64-v8a")
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }

    buildFeatures {
        compose = true
    }

    packaging {
        resources {
            excludes += setOf("META-INF/AL2.0", "META-INF/LGPL2.1")
        }
    }
}

dependencies {
    implementation(platform(libs.compose.bom))
    implementation(libs.androidx.core.ktx)
    implementation(libs.androidx.activity.compose)
    implementation(libs.androidx.lifecycle.runtime.ktx)
    implementation(libs.androidx.lifecycle.runtime.compose)
    implementation(libs.kotlinx.coroutines.android)
    implementation("net.java.dev.jna:jna:${libs.versions.jna.get()}@aar")
    implementation(libs.compose.ui)
    implementation(libs.compose.material3)
    implementation(libs.compose.tooling.preview)
    debugImplementation(libs.compose.tooling)
    testImplementation(libs.junit)
}
