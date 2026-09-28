import 'stormview/themes.css'
import { initTheme } from 'stormview/theme'
import { mount } from 'svelte'
import App from './App.svelte'

initTheme()
mount(App, { target: document.getElementById('app') })
