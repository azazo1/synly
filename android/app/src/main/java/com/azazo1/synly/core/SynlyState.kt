package com.azazo1.synly.core

import uniffi.synly_core.FfiClientState

data class PinRequest(
    val requestId: String,
    val bootstrapShort: String,
    val bootstrapRandomart: String,
    val sessionShort: String,
    val sessionRandomart: String,
)

data class BluetoothAuthorization(
    val requestId: String,
    val displayName: String,
    val deviceId: String,
    val fingerprint: String,
    val systemAddress: String,
    val changedIdentity: Boolean,
    val capabilitiesSummary: String,
)

data class SynlyUiState(
    val state: FfiClientState? = null,
    val connectedDevice: String? = null,
    val targetLabel: String? = null,
    val transportSummary: String? = null,
    val bluetoothAvailable: Boolean = false,
    val pinRequest: PinRequest? = null,
    val bluetoothAuthorization: BluetoothAuthorization? = null,
    val lastMessage: String? = null,
    val lastReceivedText: String? = null,
    val lastReceivedImagePng: ByteArray? = null,
    val canSend: Boolean = false,
    val canReceive: Boolean = false,
)
