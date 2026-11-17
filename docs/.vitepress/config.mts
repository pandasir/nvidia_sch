import { defineConfig } from 'vitepress'

export default defineConfig({
  base: '/nvidia_sch/',
  title: 'GPU Manager',
  description: '单机 GPU 调度与隔离服务：整卡独占、优先级排队、cgroup v2 隔离、审计与恢复。',
  lastUpdated: true,
  themeConfig: {
    nav: [
      { text: '首页', link: '/' },
      { text: '下载', link: '/download/' },
      { text: '快速开始', link: '/guide/quick-start/' },
      { text: 'GitHub', link: 'https://github.com/pandasir/nvidia_sch' },
      { text: 'Gitee', link: 'https://gitee.com/yydb/nvidia_sch' }
    ],
    sidebar: {
      '/guide/': [
        {
          text: '指南',
          items: [{ text: '快速开始', link: '/guide/quick-start/' }]
        }
      ]
    },
    search: {
      provider: 'local'
    },
    footer: {
      message: 'GPU Manager - 单机 GPU 调度与隔离',
      copyright: 'MIT License'
    }
  }
})
