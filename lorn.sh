#!/usr/bin/env sh
#-*-mode: Shell-script; coding: utf-8;-*-
# SPDX-License-Identifier: BlueOak-1.0.0
# Description: Bourne sh/posix compatible shell script version of what lorn
# does. Assumes a properly setup environment within which to run.
_base=$(basename "$0")
_dir=$(cd -P -- "$(dirname -- "$(command -v -- "$0")")" && pwd -P || exit 126)
export _base _dir
set "${SETOPTS:--eu}"
set -eu

found=/tmp/$$.found
trap 'rm -f "$found"' EXIT INT TERM QUIT

is_bitnami_image() {
  case "$1" in
    docker.io/bitnami/* | bitnami/*) return 0 ;;
    *) return 1 ;;
  esac
}

kubectl get pvc -A -o json | jq -r '.items[] | [.metadata.namespace, .metadata.name, .spec.volumeName] | @tsv' | while IFS=$'\t' read ns pvc pv; do
  pods=$(kubectl get pods -n "$ns" -o json | jq -r --arg c "$pvc" '.items[].spec.volumes[]? | select(.persistentVolumeClaim.claimName==$c) | .persistentVolumeClaim.claimName')
  sts=$(kubectl get sts -n "$ns" -o json | jq -r --arg c "$pvc" '.items[].spec.volumeClaimTemplates[]? | select(.metadata.name==$c) | .metadata.name')
  deps=$(kubectl get deploy -n "$ns" -o json | jq -r --arg c "$pvc" '.items[].spec.template.spec.volumes[]? | select(.persistentVolumeClaim.claimName==$c) | .persistentVolumeClaim.claimName')
  jobs=$(kubectl get jobs -n "$ns" -o json | jq -r --arg c "$pvc" '.items[].spec.template.spec.volumes[]? | select(.persistentVolumeClaim.claimName==$c) | .persistentVolumeClaim.claimName')
  if [ -z "$pods" ] && [ -z "$sts" ] && [ -z "$deps" ] && [ -z "$jobs" ]; then
    printf 'orphaned pvc %s/%s pv %s\n' "$ns" "$pvc" "$pv"
    touch "$found"
  fi
done

for ns in $(kubectl get ns -o jsonpath='{.items[*].metadata.name}'); do
  kubectl get pods -n "$ns" -o json \
    | jq -r '.items[] | . as $p | (
        ( .spec.containers // [] | to_entries[] | { path: "spec.containers[\(.key)]", image: .value.image } ),
        ( .spec.initContainers // [] | to_entries[] | { path: "spec.initContainers[\(.key)]", image: .value.image } )
      ) | "\($p.metadata.name)\t\(.path)\t\(.image)"' \
    | while IFS=$(printf '\t') read -r name path image; do
      if is_bitnami_image "$image"; then
        printf 'pod %s/%s %s.image legacy image %s\n' "$ns" "$name" "$path" "$image"
        touch "$found"
      fi
    done

  kubectl get sts -n "$ns" -o json \
    | jq -r '.items[] | . as $s |
        .spec.template.spec.containers // [] | to_entries[] |
        "\($s.metadata.name)\tspec.containers[\(.key)]\t\(.value.image)"' \
    | while IFS=$(printf '\t') read -r name path image; do
      if is_bitnami_image "$image"; then
        printf 'statefulset %s/%s %s.image legacy image %s\n' "$ns" "$name" "$path" "$image"
        touch "$found"
      fi
    done

  kubectl get deploy -n "$ns" -o json \
    | jq -r '.items[] | . as $d |
        .spec.template.spec.containers // [] | to_entries[] |
        "\($d.metadata.name)\tspec.containers[\(.key)]\t\(.value.image)"' \
    | while IFS=$(printf '\t') read -r name path image; do
      if is_bitnami_image "$image"; then
        printf 'deployment %s/%s %s.image legacy image %s\n' "$ns" "$name" "$path" "$image"
        touch "$found"
      fi
    done

  kubectl get jobs -n "$ns" -o json \
    | jq -r '.items[] | . as $j |
        .spec.template.spec.containers // [] | to_entries[] |
        "\($j.metadata.name)\tspec.containers[\(.key)]\t\(.value.image)"' \
    | while IFS=$(printf '\t') read -r name path image; do
      if is_bitnami_image "$image"; then
        printf 'job %s/%s %s.image legacy image %s\n' "$ns" "$name" "$path" "$image"
        touch "$found"
      fi
    done
done

if [ -f "$found" ]; then
  exit 1
fi

exit 0
