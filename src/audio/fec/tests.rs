use super::{encode_audio_block, recover_audio_block};
use crate::audio::protocol::{RTPA_DATA_SHARDS, RTPA_FEC_SHARDS};

// 由真实 nanors C 生成, 来源和复现命令见 docs/audio-fec-vectors.md.
// 此常量块保持生成器输出原样, 不用 Rust 编码器计算恢复测试的校验片.
const UPSTREAM_PARITY: [(&str, usize, [&str; 2]); 2] = [
    ("basis", 4, [
        "7740380e",
        "c7a70d6c",
    ]),
    ("sweep", 257, [
        "1b1a2b8406047836bf540458d1da8c7ef17bd88fef89b013dea70fb15c12a91f027031025dff73a6d54e82032ad11c14ebfd8374e419da0958fcf4bacc78b399598b3a929012bf712e4512cec71dcbefe06d4e9928ce2102c83119761b83b8099466f6459feee230c389c5c13b408a022cba4165758fccce1f3ee52b5a6e74de9b9aab048684f8b63fd484d8515a0cfe71fb580f6f0930935e278f31dc92299f82f0b182dd7ff32655ce0283aa519c946b7d03f464995a89d87c743a4cf83319d90bba1210923ff1aec5924e479d4b6f60edce19a84ea18248b199f69b03388914e676c51f6e62b043094541bbc00a82ac3ac1e5f50f4c4e9fbe65abdaeef45e1b",
        "7cf6cdd9ec5e5334d171f55f3e8749145649738d694dcb7f6ecfa1da2d1f02abe81df69ef2bbe6043a4ab241db3279ff6d0e6d68dc7d204429d1446f1df439ecf6f843ae1b296a8ddfff82a849bef01ad83e84fa50f4c5f11938d6e394118cdc1f6acf27f835e8f34d730b4b553c8e8854b767e6d28a577d90dbca61ea830055fc764d596cded3b451f175dfbe07c994d6c9f30de9cd4bffee4f215aad9f822b689d761e723b6684baca32c15bb2f97fed8eede85cfda0c4a951c4ef9d74b96c7678c32e9ba9ea0d5f7f0228c93e709a58be047ad074457199b8566314910c5c9fea4fa778b56873cdf38bcbd5bc0e08d437e766520ad7fd105b4ae16a0380d57c",
    ]),
];

fn input_shards(name: &str, size: usize) -> [Vec<u8>; RTPA_DATA_SHARDS] {
    std::array::from_fn(|shard| {
        (0..size)
            .map(|byte| match name {
                "basis" => u8::from(shard == byte),
                "sweep" => (byte * 73 + shard * 41) as u8,
                _ => panic!("unknown upstream vector: {name}"),
            })
            .collect()
    })
}

fn parity_shards(hex: [&str; RTPA_FEC_SHARDS], size: usize) -> [Vec<u8>; RTPA_FEC_SHARDS] {
    hex.map(|value| {
        assert_eq!(value.len(), size * 2);
        (0..value.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&value[index..index + 2], 16).unwrap())
            .collect()
    })
}

#[test]
fn encoding_matches_upstream_c_vectors() {
    for (name, size, hex) in UPSTREAM_PARITY {
        let data = input_shards(name, size);
        let expected = parity_shards(hex, size);
        let mut parity0 = vec![0xa5; size];
        let mut parity1 = vec![0xa5; size];
        encode_audio_block(
            std::array::from_fn(|index| data[index].as_slice()),
            [&mut parity0, &mut parity1],
        )
        .unwrap();
        assert_eq!([parity0, parity1], expected, "vector={name}");
    }
}

#[test]
fn single_data_loss_recovers_with_each_upstream_parity() {
    for (name, size, hex) in UPSTREAM_PARITY {
        let original = input_shards(name, size);
        let upstream = parity_shards(hex, size);
        for missing in 0..RTPA_DATA_SHARDS {
            // 分别只保留第 0 行, 第 1 行, 以及两行校验片.
            for available in [1u8, 2, 3] {
                let mut data = original.clone().map(Some);
                data[missing] = None;
                let parity = std::array::from_fn(|row| {
                    (available & (1 << row) != 0).then(|| upstream[row].clone())
                });
                assert_eq!(recover_audio_block(&mut data, &parity).unwrap(), 1);
                assert_eq!(data, original.clone().map(Some),
                    "vector={name}, missing={missing}, parity={available}");
            }
        }
    }
}

#[test]
fn every_double_data_loss_recovers_with_upstream_parity() {
    for (name, size, hex) in UPSTREAM_PARITY {
        let original = input_shards(name, size);
        let parity = parity_shards(hex, size).map(Some);
        for first in 0..RTPA_DATA_SHARDS {
            for second in first + 1..RTPA_DATA_SHARDS {
                let mut data = original.clone().map(Some);
                data[first] = None;
                data[second] = None;
                assert_eq!(recover_audio_block(&mut data, &parity).unwrap(), 2);
                assert_eq!(data, original.clone().map(Some),
                    "vector={name}, missing={first},{second}");
            }
        }
    }
}
