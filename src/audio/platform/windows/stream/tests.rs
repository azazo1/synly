use super::*;

#[test]
fn short_or_non_extensible_mix_headers_never_supply_a_channel_mask() {
    for (tag, cb_size) in [(3, 0), (1, 22), (EXTENSIBLE, 0), (EXTENSIBLE, 21)] {
        let header = WaveFormatEx { w_format_tag: tag, n_channels: 6,
            n_samples_per_sec: 44_100, n_avg_bytes_per_sec: 1_058_400,
            n_block_align: 24, w_bits_per_sample: 32, cb_size };
        let mut format = WasapiSpec { sample_rate: 48_000, channels: 6 }.wave_format().unwrap();
        unsafe { apply_mix_format(&mut format, &header); }
        let mask = format.channel_mask;
        assert_eq!(mask, 0x3f);
    }
}
#[test]
fn unaligned_native_header_changes_only_compatible_channel_mask() {
    for (native_channels, native_mask, expected) in [(6, 0x60f, 0x60f), (6, 0xfc, 0x3f), (8, 0x63f, 0x3f)] {
        let mut native = WasapiSpec { sample_rate: 44_100, channels: native_channels }.wave_format().unwrap();
        native.channel_mask = native_mask;
        let mut bytes = [0u8; 41];
        unsafe { ptr::copy_nonoverlapping((&native as *const WaveFormatExtensible).cast::<u8>(), bytes.as_mut_ptr().add(1), 40); }
        let mut format = WasapiSpec { sample_rate: 48_000, channels: 6 }.wave_format().unwrap();
        unsafe { apply_mix_format(&mut format, bytes.as_ptr().add(1).cast()); }
        let (mask, rate, channels, bits) = (format.channel_mask, format.format.n_samples_per_sec, format.format.n_channels, format.valid_bits);
        assert_eq!((mask, rate, channels, bits), (expected, 48_000, 6, 32));
    }
}
