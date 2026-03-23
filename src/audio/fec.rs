use crate::audio::error::{Error, Result};
use crate::audio::protocol::{RTPA_DATA_SHARDS, RTPA_FEC_SHARDS};

const PARITY_MATRIX: [[u8; RTPA_DATA_SHARDS]; RTPA_FEC_SHARDS] =
    [[0x77, 0x40, 0x38, 0x0e], [0xc7, 0xa7, 0x0d, 0x6c]];

pub fn encode_audio_block(
    data: [&[u8]; RTPA_DATA_SHARDS],
    parity: [&mut [u8]; RTPA_FEC_SHARDS],
) -> Result<()> {
    let block_len = data[0].len();
    if data.iter().any(|shard| shard.len() != block_len) {
        return Err(Error::Protocol(
            "audio FEC requires equal-sized data shards".into(),
        ));
    }
    if parity.iter().any(|shard| shard.len() != block_len) {
        return Err(Error::Protocol(
            "audio FEC parity shard length mismatch".into(),
        ));
    }

    parity[0].fill(0);
    parity[1].fill(0);

    let [parity0, parity1] = parity;
    for index in 0..block_len {
        let mut p0 = 0u8;
        let mut p1 = 0u8;
        for shard_index in 0..RTPA_DATA_SHARDS {
            let value = data[shard_index][index];
            p0 ^= gf_mul(PARITY_MATRIX[0][shard_index], value);
            p1 ^= gf_mul(PARITY_MATRIX[1][shard_index], value);
        }
        parity0[index] = p0;
        parity1[index] = p1;
    }

    Ok(())
}

pub fn recover_audio_block(
    data: &mut [Option<Vec<u8>>; RTPA_DATA_SHARDS],
    parity: &[Option<Vec<u8>>; RTPA_FEC_SHARDS],
) -> Result<usize> {
    let block_len = shard_len(data, parity)?;
    let missing: Vec<usize> = data
        .iter()
        .enumerate()
        .filter_map(|(index, shard)| shard.is_none().then_some(index))
        .collect();

    if missing.is_empty() {
        return Ok(0);
    }
    if missing.len() > RTPA_FEC_SHARDS {
        return Err(Error::Protocol(
            "too many missing audio shards for FEC recovery".into(),
        ));
    }

    let available_parity: Vec<(usize, &Vec<u8>)> = parity
        .iter()
        .enumerate()
        .filter_map(|(index, shard)| shard.as_ref().map(|data| (index, data)))
        .collect();

    if available_parity.len() < missing.len() {
        return Err(Error::Protocol(
            "not enough parity shards for audio recovery".into(),
        ));
    }

    match missing.len() {
        1 => recover_single(data, &missing, &available_parity, block_len),
        2 => recover_double(data, &missing, &available_parity, block_len),
        _ => unreachable!(),
    }
}

fn recover_single(
    data: &mut [Option<Vec<u8>>; RTPA_DATA_SHARDS],
    missing: &[usize],
    available_parity: &[(usize, &Vec<u8>)],
    block_len: usize,
) -> Result<usize> {
    let missing_idx = missing[0];
    let (parity_row, parity_bytes) = available_parity[0];
    let coeff = PARITY_MATRIX[parity_row][missing_idx];
    if coeff == 0 {
        return Err(Error::Protocol(
            "singular parity coefficient for audio recovery".into(),
        ));
    }

    let mut recovered = vec![0u8; block_len];
    for byte_index in 0..block_len {
        let mut rhs = parity_bytes[byte_index];
        for (shard_index, shard) in data.iter().enumerate() {
            if shard_index == missing_idx {
                continue;
            }
            let shard = shard.as_ref().ok_or_else(|| {
                Error::Protocol("unexpected missing shard during single recovery".into())
            })?;
            rhs ^= gf_mul(PARITY_MATRIX[parity_row][shard_index], shard[byte_index]);
        }
        recovered[byte_index] = gf_div(rhs, coeff)?;
    }

    data[missing_idx] = Some(recovered);
    Ok(1)
}

fn recover_double(
    data: &mut [Option<Vec<u8>>; RTPA_DATA_SHARDS],
    missing: &[usize],
    available_parity: &[(usize, &Vec<u8>)],
    block_len: usize,
) -> Result<usize> {
    let first_missing = missing[0];
    let second_missing = missing[1];
    let (row0, parity0) = available_parity[0];
    let (row1, parity1) = available_parity[1];

    let a00 = PARITY_MATRIX[row0][first_missing];
    let a01 = PARITY_MATRIX[row0][second_missing];
    let a10 = PARITY_MATRIX[row1][first_missing];
    let a11 = PARITY_MATRIX[row1][second_missing];
    let det = gf_mul(a00, a11) ^ gf_mul(a01, a10);

    if det == 0 {
        return Err(Error::Protocol(
            "singular 2x2 audio FEC recovery matrix".into(),
        ));
    }

    let mut recovered0 = vec![0u8; block_len];
    let mut recovered1 = vec![0u8; block_len];

    for byte_index in 0..block_len {
        let mut b0 = parity0[byte_index];
        let mut b1 = parity1[byte_index];
        for (shard_index, shard) in data.iter().enumerate() {
            if shard_index == first_missing || shard_index == second_missing {
                continue;
            }
            let shard = shard.as_ref().ok_or_else(|| {
                Error::Protocol("unexpected missing shard during double recovery".into())
            })?;
            b0 ^= gf_mul(PARITY_MATRIX[row0][shard_index], shard[byte_index]);
            b1 ^= gf_mul(PARITY_MATRIX[row1][shard_index], shard[byte_index]);
        }

        let x0 = gf_div(gf_mul(b0, a11) ^ gf_mul(a01, b1), det)?;
        let x1 = gf_div(gf_mul(a00, b1) ^ gf_mul(b0, a10), det)?;
        recovered0[byte_index] = x0;
        recovered1[byte_index] = x1;
    }

    data[first_missing] = Some(recovered0);
    data[second_missing] = Some(recovered1);
    Ok(2)
}

fn shard_len(
    data: &[Option<Vec<u8>>; RTPA_DATA_SHARDS],
    parity: &[Option<Vec<u8>>; RTPA_FEC_SHARDS],
) -> Result<usize> {
    let mut len = None;

    for shard in data.iter().chain(parity.iter()).flatten() {
        match len {
            Some(existing) if existing != shard.len() => {
                return Err(Error::Protocol(
                    "inconsistent audio shard sizes in FEC block".into(),
                ));
            }
            Some(_) => {}
            None => len = Some(shard.len()),
        }
    }

    len.ok_or_else(|| Error::Protocol("no shards available in FEC block".into()))
}

fn gf_mul(mut a: u8, mut b: u8) -> u8 {
    let mut product = 0u8;
    while b != 0 {
        if b & 1 != 0 {
            product ^= a;
        }
        let carry = a & 0x80;
        a <<= 1;
        if carry != 0 {
            a ^= 0x1d;
        }
        b >>= 1;
    }
    product
}

fn gf_pow(mut value: u8, mut exponent: u16) -> u8 {
    let mut acc = 1u8;
    while exponent != 0 {
        if exponent & 1 != 0 {
            acc = gf_mul(acc, value);
        }
        value = gf_mul(value, value);
        exponent >>= 1;
    }
    acc
}

fn gf_inv(value: u8) -> Result<u8> {
    if value == 0 {
        return Err(Error::Protocol("cannot invert zero in GF(256)".into()));
    }
    Ok(gf_pow(value, 254))
}

fn gf_div(lhs: u8, rhs: u8) -> Result<u8> {
    Ok(gf_mul(lhs, gf_inv(rhs)?))
}
