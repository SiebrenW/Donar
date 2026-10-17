#!/bin/sh
# Runs the comparison and writes RESULTS.md (the tables). From anywhere:
#   crates/codec/exi-gen/benchmarks/run.sh
# Needs: `cargo xtask setup && cargo xtask vectors` (the schemas, the reference vectors, the EXIficient jars),
# a JDK, a C compiler, and clones of the two C libraries in data/bench:
#   git clone https://github.com/Martin-P/OpenV2G data/bench/OpenV2G
#   git clone https://github.com/EVerest/libcbv2g data/bench/libcbv2g
set -e
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../../.." && pwd)
build=$here/build
mkdir -p "$build"
[ -d /opt/homebrew/opt/openjdk/bin ] && PATH=/opt/homebrew/opt/openjdk/bin:$PATH
RUNS=${RUNS:-5}

python3 "$here/corpus.py"
"$here/c/build.sh"

# Rust: speed (release, all protocols) and size (opt-level s, LTO, one protocol at a time).
cd "$root"
cargo build --release -q -p exi-bench
cargo_size() { # features
  cargo build --release -q -p exi-bench --no-default-features ${1:+--features "$1"} --target-dir "$build/rust-size-$2" \
    --config 'profile.release.opt-level="s"' --config profile.release.lto=true --config profile.release.codegen-units=1 --config 'profile.release.panic="abort"' --config 'profile.release.strip="none"'
}
cargo_size "" none; cargo_size din din; cargo_size iso2 iso2; cargo_size iso20 iso20

# Java (EXIficient)
mkdir -p "$build/java"
javac -cp "data/vectors/lib/*" -d "$build/java" "$here/java/Bench.java"

# Timings: RUNS runs of each, the report takes the fastest of each row.
: > "$build/results.tsv"
for i in $(seq 1 "$RUNS"); do
  "$root/target/release/exi-bench" "$here/corpus.txt" >> "$build/results.tsv"
  "$build/openv2g" "$here/corpus.txt" >> "$build/results.tsv"
  "$build/cbv2g" "$here/corpus.txt" >> "$build/results.tsv"
done
# The JVM is slow to start: two runs.
for i in 1 2; do
  java -cp "data/vectors/lib/*:$build/java" Bench "$here/corpus.txt" "$root/crates/xtask/vectors/manifest.txt" "$root/data/vectors/schemas" 2>/dev/null >> "$build/results.tsv"
done

{
  echo "date	$(date -u +%Y-%m-%dT%H:%MZ)"
  echo "machine	$(uname -srm)"
  echo "cpu	$( (sysctl -n machdep.cpu.brand_string 2>/dev/null) || (grep -m1 'model name' /proc/cpuinfo | cut -d: -f2) )"
  echo "rustc	$(rustc --version)"
  echo "cc	$(${CC:-cc} --version | head -1)"
  echo "java	$(java -version 2>&1 | head -1)"
  echo "openv2g	$(git -C "$root/data/bench/OpenV2G" log -1 --format=%h)"
  echo "cbv2g	$(git -C "$root/data/bench/libcbv2g" log -1 --format=%h)"
  echo "exificient	1.0.7"
} > "$build/env.tsv"
python3 "$here/report.py" "$build" "$here/corpus.txt" > "$here/RESULTS.md"
echo "wrote $here/RESULTS.md"
