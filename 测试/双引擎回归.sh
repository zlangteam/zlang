#!/bin/bash
# 双引擎回归：把 测试/智语言代码项目/源码 下的全部 .智 用例分别以解释模式（运行）和
# 编译模式（构建运行）执行，逐个比较「退出码」与「❌ 行数」，并列出输出文本不同的用例。
#
# 用法（在仓库根目录）：
#   cargo build --bin 智语言开发套件
#   测试/双引擎回归.sh [结果目录] [并行数]
#
# 退出码：0 = 两种模式逐例一致且没有 ❌；1 = 存在不一致或失败用例。
# 说明：
#   · 负向用例（文件名含「负向」等）本来就以非零退出，属正常；这里只要求两种模式一致。
#   · 输出文本差异目前仍有若干已知项（异常位置/堆栈格式、标准错误先后顺序、
#     随机值与时间戳），只列出、不计入失败。
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN="$ROOT/产出/debug/智语言开发套件"
OUT=${1:-"$ROOT/产出/双引擎回归"}
JOBS=${2:-6}
[ -x "$BIN" ] || { echo "未找到 $BIN，请先 cargo build --bin 智语言开发套件"; exit 2; }
mkdir -p "$OUT/解释" "$OUT/编译"
cd "$ROOT/测试/智语言代码项目" || exit 2

run_one() {
  MODE=$1; DIR=$2; FILE=$3; NAME=$(basename "$FILE" .智)
  timeout 120 "$BIN" "$MODE" "$FILE" > "$DIR/$NAME.log" 2>&1 < /dev/null
  echo "EXIT=$?" >> "$DIR/$NAME.log"
}
export -f run_one; export BIN

run_all() {
  MODE=$1; DIR=$2
  rm -f "$DIR"/*.log
  ls 源码/*.智 | xargs -P "$JOBS" -I{} bash -c 'run_one "$0" "$1" "$2"' "$MODE" "$DIR" {}
  for LOG in "$DIR"/*.log; do
    CODE=$(grep -a '^EXIT=' "$LOG" | tail -1 | cut -d= -f2)
    FAILS=$(grep -a -c '❌' "$LOG")
    printf '%s\t%s\t%s\n' "$(basename "$LOG" .log)" "$CODE" "$FAILS"
  done | sort > "$DIR/汇总.tsv"
}

echo "[1/2] 解释模式…"; run_all 运行 "$OUT/解释"
echo "[2/2] 编译模式…"; run_all 构建运行 "$OUT/编译"

flt() { grep -av '^\[智语言\]\|字节码：\|编译完成' "$1"; }
TOTAL=$(wc -l < "$OUT/解释/汇总.tsv")
MISMATCH=$(join -t$'\t' "$OUT/解释/汇总.tsv" "$OUT/编译/汇总.tsv" | awk -F'\t' '$2!=$4 || $3!=$5')
FAILED=$(awk -F'\t' '$3>0 {print $1}' "$OUT/解释/汇总.tsv" "$OUT/编译/汇总.tsv" | sort -u)
: > "$OUT/输出差异.txt"
for LOG in "$OUT/解释"/*.log; do
  NAME=$(basename "$LOG")
  diff -q <(flt "$LOG") <(flt "$OUT/编译/$NAME") > /dev/null 2>&1 || echo "${NAME%.log}" >> "$OUT/输出差异.txt"
done

echo "用例总数：$TOTAL"
echo "退出码或失败数双模不一致：$(printf '%s' "$MISMATCH" | grep -c .)"
[ -n "$MISMATCH" ] && printf '%s\n' "$MISMATCH" | sed 's/^/  /'
echo "含 ❌ 的用例：$(printf '%s' "$FAILED" | grep -c .)"
[ -n "$FAILED" ] && printf '%s\n' "$FAILED" | sed 's/^/  /'
echo "输出文本双模不同（仅提示）：$(wc -l < "$OUT/输出差异.txt")，清单见 $OUT/输出差异.txt"
[ -z "$MISMATCH" ] && [ -z "$FAILED" ]
