import DefaultTheme from 'vitepress/theme'
import ReleaseDownload from './components/ReleaseDownload.vue'
import './custom.css'

export default {
  extends: DefaultTheme,
  enhanceApp({ app }) {
    app.component('ReleaseDownload', ReleaseDownload)
  }
}
