#!/usr/bin/env python3
"""独立校验 bod-canonical-json-v1 测试向量（跨语言一致性证据）。

本脚本刻意不引用 Rust 实现，只用标准库按 v1 规则复算：

- 键按 Unicode 码点升序（Python 的 sort_keys 与 Rust 的 UTF-8 字节序对合法 UTF-8 等价）
- 分隔符为 `,` 与 `:`，无空白
- 非 ASCII 原样输出（ensure_ascii=False）
- 只接受整数；小数与指数一律拒绝
- 同一对象内重复键拒绝
- 规范摘要与原始字节摘要分别计算

用法：
    python verification/vectors/check_bod_canonical_json_v1.py
退出码 0 表示全部向量一致；非 0 表示存在不一致或回归。
"""

import hashlib
import io
import json
import re
import sys

VECTOR_PATH = "verification/vectors/bod-canonical-json-v1.json"
CANONICALIZATION = "bod-canonical-json-v1"
INT_LITERAL = re.compile(r"^(0|-?[1-9][0-9]*)$")


class StrictError(Exception):
    """向量声明为必须拒绝的输入；`code` 与 Rust 侧稳定分类名一一对应。"""

    def __init__(self, code):
        super().__init__(code)
        self.code = code


def _no_duplicates(pairs):
    seen = set()
    for key, _ in pairs:
        if key in seen:
            raise StrictError("duplicate_key")
        seen.add(key)
    return dict(pairs)


def _strict_int(text):
    if not INT_LITERAL.match(text):
        raise StrictError("non_canonical_number")
    return int(text)


def _strict_float(text):
    lowered = text.lower()
    if "." in lowered:
        raise StrictError("fractional_number")
    raise StrictError("non_canonical_number")


def strict_loads(text):
    return json.loads(
        text,
        object_pairs_hook=_no_duplicates,
        parse_int=_strict_int,
        parse_float=_strict_float,
        parse_constant=lambda name: (_ for _ in ()).throw(StrictError("non_finite_number")),
    )


def canonical_text(obj):
    return json.dumps(obj, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def sha256_hex(text):
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def check_case(case):
    name = case["name"]
    raw = case["input"]

    if "expected_error" in case:
        expected = case["expected_error"]
        try:
            obj = strict_loads(raw)
        except StrictError as err:
            if err.code != expected:
                raise AssertionError(
                    "%s 拒绝分类不一致：向量 %s，Python %s" % (name, expected, err.code)
                )
            return "%s: rejected (%s)" % (name, err.code)
        # 单独处理孤立代理项：Python 解析放行，但写 UTF-8 时失败。
        if expected == "invalid_json":
            try:
                canonical_text(obj).encode("utf-8")
            except UnicodeEncodeError:
                return "%s: rejected (encode)" % name
            raise AssertionError("%s 应被拒绝，但 Python 能完整编码" % name)
        if expected == "not_an_object":
            if not isinstance(obj, dict):
                return "%s: rejected (top-level type)" % name
        raise AssertionError("%s 应被拒绝，但严格解析成功" % name)

    obj = strict_loads(raw)
    if not isinstance(obj, dict):
        raise AssertionError("%s 顶层必须是对象" % name)
    canonical = canonical_text(obj)
    if canonical != case["canonical"]:
        raise AssertionError(
            "%s 规范文本不一致\n  期望: %s\n  实得: %s" % (name, case["canonical"], canonical)
        )
    digest = sha256_hex(canonical)
    if digest != case["sha256"]:
        raise AssertionError(
            "%s 规范摘要不一致\n  期望: %s\n  实得: %s" % (name, case["sha256"], digest)
        )
    if "raw_sha256" in case:
        raw_digest = sha256_hex(raw)
        if raw_digest != case["raw_sha256"]:
            raise AssertionError(
                "%s 原始字节摘要不一致\n  期望: %s\n  实得: %s"
                % (name, case["raw_sha256"], raw_digest)
            )
    return "%s: ok sha256=%s" % (name, digest[:16])


def main():
    with io.open(VECTOR_PATH, encoding="utf-8") as fh:
        doc = json.load(fh)
    assert doc["canonicalization"] == CANONICALIZATION, "向量文件声明了不同规范版本"

    failures = []
    for case in doc["vectors"]:
        try:
            print(check_case(case))
        except AssertionError as err:
            failures.append(str(err))

    total = len(doc["vectors"])
    if failures:
        print("\nFAILED %d/%d" % (len(failures), total))
        for item in failures:
            print(" -", item)
        return 1
    print("\nPASS %d/%d vectors (canonicalization=%s)" % (total, total, CANONICALIZATION))
    return 0


if __name__ == "__main__":
    sys.exit(main())
