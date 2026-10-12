// The design system's styles.css, imported part by part so the self-host plugin
// (vite.config.ts) can strip the Google Fonts @import from tokens/fonts.css.
import '@hryer/madlabs-design-system/tokens/fonts.css';
import '@hryer/madlabs-design-system/tokens/colors.css';
import '@hryer/madlabs-design-system/tokens/typography.css';
import '@hryer/madlabs-design-system/tokens/spacing.css';
import '@hryer/madlabs-design-system/tokens/effects.css';
import '@hryer/madlabs-design-system/tokens/motion.css';
import '@hryer/madlabs-design-system/tokens/base.css';
import '@hryer/madlabs-design-system/components.css';
import './app.css';
import { mount } from 'svelte';
import App from './App.svelte';

mount(App, { target: document.getElementById('app')! });
