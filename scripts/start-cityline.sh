#!/usr/bin/env bash
set -euo pipefail
umask 077

cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
workspace="$(pwd -P)"
config="${CITYLINE_ENV_FILE:-$workspace/.env.local}"
binary="${1:-$workspace/apps/acceptance-rust/target/release/hackson-acceptance}"

if [[ ! -f "$config" && -n "${CITYLINE_ENV_FILE:-}" ]]; then
  printf '无法读取指定的城线环境配置。\n' >&2
  exit 1
fi
if [[ -f "$config" ]]; then
  declare -A seen=()
  number=0
  field_pattern='^((CITYLINE|ACCEPTANCE_RUST)_[A-Z0-9_]+)=(.*)$'
  while IFS= read -r line || [[ -n "$line" ]]; do
    number=$((number + 1))
    line="${line#"${line%%[![:space:]]*}"}"
    line="${line%"${line##*[![:space:]]}"}"
    [[ -z "$line" || "$line" == '#'* ]] && continue
    if [[ ! "$line" =~ $field_pattern ]]; then
      printf '城线环境配置第 %s 行须使用项目变量名及等号。\n' "$number" >&2
      exit 1
    fi
    key="${BASH_REMATCH[1]}"
    value="${BASH_REMATCH[3]}"
    value="${value#"${value%%[![:space:]]*}"}"
    if [[ -n "${seen[$key]:-}" ]]; then
      printf '城线环境配置第 %s 行的变量重复。\n' "$number" >&2
      exit 1
    fi
    seen[$key]=1
    if [[ -z "${!key:-}" && -n "$value" ]]; then
      export "$key=$value"
    fi
  done < "$config"
fi

if [[ ! -f "$binary" || ! -x "$binary" ]]; then
  printf '请先构建城线服务，或指定已构建的可执行文件。\n' >&2
  exit 1
fi
exec "$binary"
