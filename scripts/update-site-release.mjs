#!/usr/bin/env node

import fs from 'node:fs'

function parseArgs(argv) {
  const args = {}
  for (let i = 0; i < argv.length; i += 2) {
    const key = argv[i].replace(/^--/, '')
    args[key] = argv[i + 1]
  }
  return args
}

function compareSemver(left, right) {
  const a = left.split('-')[0].split('.').map(Number)
  const b = right.split('-')[0].split('.').map(Number)
  for (let i = 0; i < 3; i += 1) {
    const diff = (a[i] || 0) - (b[i] || 0)
    if (diff !== 0) return diff
  }
  return 0
}

const args = parseArgs(process.argv.slice(2))
const tag = args.tag
if (!tag || !/^v\d+\.\d+\.\d+(?:-[0-9A-Za-z.]+)?$/.test(tag)) {
  console.error('usage: update-site-release.mjs --tag v0.1.0 [--prerelease false] [--gitee-available true] [--manifest path]')
  process.exit(2)
}

const manifestPath = args.manifest || 'docs/public/release.json'
const current = JSON.parse(fs.readFileSync(manifestPath, 'utf8'))
const version = tag.slice(1)
const prerelease = args.prerelease === 'true' || /-[0-9A-Za-z.]+$/.test(tag)
const giteeAvailable = args['gitee-available'] !== 'false'

if (prerelease) {
  console.log(`skip prerelease ${tag}; stable version remains ${current.version}`)
  process.exit(0)
}

const cmp = compareSemver(version, current.version)
if (cmp < 0) {
  console.log(`skip older ${tag}; stable version remains ${current.version}`)
  process.exit(0)
}

const next = {
  version,
  tag,
  prerelease: false,
  github: {
    archive: `https://github.com/pandasir/nvidia_sch/releases/download/${tag}/gpu-manager-linux-amd64-${version}.tar.gz`,
    latest: 'https://github.com/pandasir/nvidia_sch/releases/latest/download/gpu-manager-linux-amd64.tar.gz'
  },
  gitee: {
    archive: `https://gitee.com/yydb/nvidia_sch/releases/download/${tag}/gpu-manager-linux-amd64-${version}.tar.gz`,
    available: giteeAvailable
  },
  sha256sums: `https://github.com/pandasir/nvidia_sch/releases/download/${tag}/SHA256SUMS`
}

fs.writeFileSync(manifestPath, `${JSON.stringify(next, null, 2)}\n`)
console.log(`updated site release manifest to ${tag} (gitee.available=${giteeAvailable})`)
