#!/usr/bin/env python3
"""验证分发 APK 使用当前正式 keystore 的签名证书, 不输出私钥或密码."""
import argparse
import hashlib
import logging
import os
from pathlib import Path
import re
import subprocess
import sys


def required_env(name: str) -> str:
    value = os.environ.get(name, "")
    if not value.strip():
        raise RuntimeError(f"缺少签名配置: {name}")
    return value


def run_tool(arguments: list[str]) -> bytes:
    tool = Path(arguments[0]).name
    try:
        result = subprocess.run(arguments, capture_output=True, check=False, timeout=60)
    except subprocess.TimeoutExpired:
        raise RuntimeError(f"{tool} 执行超时") from None
    except OSError:
        raise RuntimeError(f"无法启动 {tool}") from None
    if result.returncode:
        # 工具输出可能包含 keystore 路径和别名, 只记录工具与退出码.
        raise RuntimeError(f"{tool} 执行失败, 退出码: {result.returncode}")
    return result.stdout


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("apk", type=Path)
    parser.add_argument("--apksigner", required=True)
    args = parser.parse_args()
    try:
        if not args.apk.is_file():
            raise RuntimeError("待验证 APK 不存在")
        keystore = required_env("SYNLY_ANDROID_KEYSTORE_FILE")
        alias = required_env("SYNLY_ANDROID_KEY_ALIAS")
        required_env("SYNLY_ANDROID_KEYSTORE_PASSWORD")
        # 只导出公开证书的 DER, 密码通过环境变量名传递, 不进入命令行参数.
        certificate = run_tool([
            "keytool", "-exportcert", "-keystore", keystore, "-alias", alias,
            "-storepass:env", "SYNLY_ANDROID_KEYSTORE_PASSWORD",
        ])
        if not certificate:
            raise RuntimeError("正式签名证书为空")
        expected = hashlib.sha256(certificate).hexdigest()
        verification = run_tool([args.apksigner, "verify", "--print-certs", str(args.apk)]).decode("utf-8", errors="replace")
        digests = re.findall(
            r"^Signer #\d+ certificate SHA-256 digest:\s*([0-9a-fA-F]{64})\s*$",
            verification, re.MULTILINE,
        )
        if len(digests) != 1 or digests[0].lower() != expected:
            raise RuntimeError("APK 签名证书与正式 keystore 不一致, 已拒绝上传")
        logging.info("APK 正式签名证书校验通过, SHA256=%s", expected)
        return 0
    except (OSError, RuntimeError, UnicodeError, subprocess.TimeoutExpired) as error:
        logging.error("APK 签名校验失败: %s", error)
        return 1


if __name__ == "__main__":
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(encoding="utf-8")
    logging.basicConfig(level=logging.INFO, format="%(message)s", stream=sys.stdout)
    sys.exit(main())
