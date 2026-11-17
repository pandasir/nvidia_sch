<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'

interface ReleaseInfo {
  version: string
  tag: string
  prerelease: boolean
  github: {
    archive: string
    latest: string
  }
  gitee: {
    archive: string
    available: boolean
  }
  sha256sums: string
}

const release = ref<ReleaseInfo | null>(null)
const error = ref('')
const copied = ref(false)
const copyError = ref('')

const checksumCommand = computed(() => {
  if (!release.value) return ''
  return [
    `curl -fL -O ${release.value.sha256sums}`,
    'sha256sum -c SHA256SUMS --ignore-missing'
  ].join('\n')
})

async function copyChecksumCommand() {
  try {
    await navigator.clipboard.writeText(checksumCommand.value)
    copied.value = true
    copyError.value = ''
    setTimeout(() => {
      copied.value = false
    }, 2000)
  } catch {
    copyError.value = '复制失败，请手动选择命令。'
  }
}

onMounted(async () => {
  try {
    const response = await fetch(`${import.meta.env.BASE_URL}release.json`)
    if (!response.ok) {
      throw new Error(`HTTP ${response.status}`)
    }
    release.value = await response.json()
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
  }
})
</script>

<template>
  <section v-if="release" class="release-panel">
    <p class="release-badge">当前稳定版 {{ release.version }} · tag {{ release.tag }}</p>

    <div class="release-sources">
      <div class="release-source">
        <h3>GitHub Release</h3>
        <a class="release-button primary" :href="release.github.archive" target="_blank" rel="noopener">
          下载固定版本归档
        </a>
        <a class="release-text-link" :href="release.github.latest" target="_blank" rel="noopener">
          latest 固定地址
        </a>
      </div>

      <div v-if="release.gitee.available" class="release-source">
        <h3>Gitee 镜像</h3>
        <a class="release-button" :href="release.gitee.archive" target="_blank" rel="noopener">
          下载同版本归档
        </a>
      </div>
      <div v-else class="release-source release-unavailable">
        <h3>Gitee 镜像</h3>
        <p>镜像同步中，暂不提供下载按钮。</p>
      </div>
    </div>

    <div class="release-checksum">
      <h3>校验归档</h3>
      <pre><code>{{ checksumCommand }}</code></pre>
      <div class="checksum-actions">
        <button type="button" class="copy-button" @click="copyChecksumCommand">复制命令</button>
        <span v-if="copied" class="copy-status">已复制</span>
        <span v-if="copyError" class="copy-error">{{ copyError }}</span>
      </div>
    </div>
  </section>

  <p v-else-if="error" class="release-error">版本清单加载失败：{{ error }}</p>
  <p v-else class="release-loading">正在加载版本信息...</p>
</template>
