package com.azazo1.synly.core

import uniffi.synly_core.FfiClientState

data class PinRequest(
    val requestId: String,
    val bootstrapShort: String,
    val bootstrapRandomart: String,
    val sessionShort: String,
    val sessionRandomart: String,
)

data class SynlyUiState(
    val state: FfiClientState? = null,
    val connectedDevice: String? = null,
    val targetLabel: String? = null,
    val pinRequest: PinRequest? = null,
    val lastMessage: String? = null,
    val lastReceivedText: String? = null,
    val lastReceivedImagePng: ByteArray? = null,
    val canSend: Boolean = false,
    val canReceive: Boolean = false,
)
