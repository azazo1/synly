use anyhow::{Context, Result, bail};

const KIB: u128 = 1024;
const MIB: u128 = KIB * 1024;
const GIB: u128 = MIB * 1024;
const TIB: u128 = GIB * 1024;
const PIB: u128 = TIB * 1024;
const EIB: u128 = PIB * 1024;

pub fn parse_human_bytes(input: &str) -> Result<u64> {
    let input = input.trim();
    if input.is_empty() {
        bail!("字节大小不能为空");
    }
    let number_end = input
        .find(|ch: char| !(ch.is_ascii_digit() || ch == '.'))
        .unwrap_or(input.len());
    let number = &input[..number_end];
    if number.is_empty() {
        bail!("字节大小缺少数值");
    }
    if number.chars().filter(|ch| *ch == '.').count() > 1 {
        bail!("字节大小数值格式无效");
    }
    let suffix = input[number_end..].trim_start();
    let factor = parse_unit(suffix)?;
    let bytes = decimal_to_bytes(number, factor)?;
    Ok(bytes)
}

fn parse_unit(raw: &str) -> Result<u128> {
    let unit = raw
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace())
        .flat_map(|ch| ch.to_lowercase())
        .collect::<String>();
    match unit.as_str() {
        "" | "b" | "byte" | "bytes" => Ok(1),
        "k" | "kb" | "kib" | "kbyte" | "kbytes" | "kibyte" | "kibytes" | "kilobyte"
        | "kilobytes" | "kibibyte" | "kibibytes" => Ok(KIB),
        "m" | "mb" | "mib" | "mbyte" | "mbytes" | "mibyte" | "mibytes" | "megabyte"
        | "megabytes" | "mebibyte" | "mebibytes" => Ok(MIB),
        "g" | "gb" | "gib" | "gbyte" | "gbytes" | "gibyte" | "gibytes" | "gigabyte"
        | "gigabytes" | "gibibyte" | "gibibytes" => Ok(GIB),
        "t" | "tb" | "tib" | "tbyte" | "tbytes" | "tibyte" | "tibytes" | "terabyte"
        | "terabytes" | "tebibyte" | "tebibytes" => Ok(TIB),
        "p" | "pb" | "pib" | "pbyte" | "pbytes" | "pibyte" | "pibytes" | "petabyte"
        | "petabytes" | "pebibyte" | "pebibytes" => Ok(PIB),
        "e" | "eb" | "eib" | "ebyte" | "ebytes" | "eibyte" | "eibytes" | "exabyte"
        | "exabytes" | "exbibyte" | "exbibytes" => Ok(EIB),
        _ => bail!("字节大小单位无法识别: {raw}"),
    }
}

fn decimal_to_bytes(raw: &str, factor: u128) -> Result<u64> {
    let (integer_part, fraction_part) = match raw.split_once('.') {
        Some((integer, fraction)) => (integer, fraction),
        None => (raw, ""),
    };
    if integer_part.is_empty() && fraction_part.is_empty() {
        bail!("字节大小数值格式无效");
    }
    let integer = if integer_part.is_empty() {
        0
    } else {
        integer_part
            .parse::<u128>()
            .context("字节大小数值格式无效")?
    };
    let fraction = if fraction_part.is_empty() {
        0
    } else {
        fraction_part
            .parse::<u128>()
            .context("字节大小数值格式无效")?
    };
    let denominator = if fraction_part.is_empty() {
        1
    } else {
        let digits = fraction_part.len().try_into().context("小数位数过多")?;
        10u128.checked_pow(digits).context("小数位数过多")?
    };
    let numerator = integer
        .checked_mul(denominator)
        .and_then(|value| value.checked_add(fraction))
        .context("字节大小数值过大")?;
    let scaled = numerator
        .checked_mul(factor)
        .context("字节大小超出可表示范围")?;
    let rounded = scaled
        .checked_add(denominator / 2)
        .context("字节大小超出可表示范围")?;
    let bytes = rounded / denominator;
    if bytes > u64::MAX as u128 {
        bail!("字节大小超出可表示范围");
    }
    Ok(bytes as u64)
}

pub fn format_human_bytes(bytes: u64) -> String {
    const UNITS: [(&str, u128); 7] = [
        ("EiB", EIB),
        ("PiB", PIB),
        ("TiB", TIB),
        ("GiB", GIB),
        ("MiB", MIB),
        ("KiB", KIB),
        ("B", 1),
    ];
    let value = bytes as u128;
    for (suffix, factor) in UNITS {
        if value >= factor {
            let whole = value / factor;
            let remainder = value % factor;
            if remainder == 0 {
                return format!("{whole} {suffix}");
            }
            let tenths = (remainder * 10 + factor / 2) / factor;
            if tenths >= 10 {
                let rounded = whole + 1;
                if rounded
                    .checked_mul(factor)
                    .is_some_and(|value| value <= u64::MAX as u128)
                {
                    return format!("{rounded} {suffix}");
                }
                return format!("{whole}.9 {suffix}");
            }
            return format!("{whole}.{tenths} {suffix}");
        }
    }
    format!("{value} B")
}

#[cfg(test)]
mod tests {
    use super::{format_human_bytes, parse_human_bytes};

    #[test]
    fn parses_bare_bytes() {
        assert_eq!(parse_human_bytes("104857600").unwrap(), 104857600);
        assert_eq!(parse_human_bytes(" 0 ").unwrap(), 0);
    }

    #[test]
    fn parses_binary_units_leniently() {
        assert_eq!(parse_human_bytes("100 MiB").unwrap(), 100 * 1024 * 1024);
        assert_eq!(parse_human_bytes("100mib").unwrap(), 100 * 1024 * 1024);
        assert_eq!(parse_human_bytes(" 100 MIB ").unwrap(), 100 * 1024 * 1024);
        assert_eq!(parse_human_bytes("1K").unwrap(), 1024);
        assert_eq!(parse_human_bytes("1KB").unwrap(), 1024);
        assert_eq!(parse_human_bytes("1KiB").unwrap(), 1024);
        assert_eq!(parse_human_bytes("2 GB").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_human_bytes("3TiB").unwrap(), 3 * 1024 * 1024 * 1024 * 1024);
    }

    #[test]
    fn parses_decimal_values() {
        assert_eq!(parse_human_bytes("1.5 GiB").unwrap(), 1610612736);
        assert_eq!(parse_human_bytes(".5 MiB").unwrap(), 512 * 1024);
        assert_eq!(parse_human_bytes("1.").unwrap(), 1);
    }

    #[test]
    fn rejects_invalid_input() {
        for value in ["", "MiB", "1.2.3 MiB", "-1 KiB", "1 banana", "999999999999999999999 EiB"] {
            assert!(parse_human_bytes(value).is_err(), "{value} should fail");
        }
    }

    #[test]
    fn formats_human_readable_values() {
        assert_eq!(format_human_bytes(0), "0 B");
        assert_eq!(format_human_bytes(1024), "1 KiB");
        assert_eq!(format_human_bytes(1500), "1.5 KiB");
        assert_eq!(format_human_bytes(104857600), "100 MiB");
        assert_eq!(format_human_bytes(1572864), "1.5 MiB");
    }

    #[test]
    fn formatted_values_round_trip() {
        for value in [0, 1, 1023, 1024, 1536, 104857600, 1572864] {
            let text = format_human_bytes(value);
            assert_eq!(parse_human_bytes(&text).unwrap(), value);
        }
    }
}
