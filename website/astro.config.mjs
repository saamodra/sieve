// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

export default defineConfig({
	site: 'https://sieve.bysm.dev',
	integrations: [
		starlight({
			title: 'sieve',
			description:
				'An HTTP/HTTPS debug proxy for Android and iOS, written in Rust. Inspect traffic from emulators, Simulators, and phones.',
			logo: {
				light: './src/assets/logo-light.svg',
				dark: './src/assets/logo-dark.svg',
				alt: 'sieve logo: a disc with seven round holes',
			},
			components: {
				PageTitle: './src/components/PageTitle.astro',
			},
			customCss: [
				'@fontsource/ibm-plex-sans/400.css',
				'@fontsource/ibm-plex-sans/500.css',
				'@fontsource/ibm-plex-sans/600.css',
				'@fontsource/ibm-plex-mono/400.css',
				'@fontsource/ibm-plex-mono/500.css',
				'./src/styles/custom.css',
			],
			head: [
				{
					// Dark is the default theme; a stored choice from the theme picker still wins.
					tag: 'script',
					content:
						"try{if(!localStorage.getItem('starlight-theme')){localStorage.setItem('starlight-theme','dark');document.documentElement.dataset.theme='dark'}}catch(e){}",
				},
			],
			social: [
				{ icon: 'github', label: 'GitHub', href: 'https://github.com/saamodra/sieve' },
			],
			editLink: {
				baseUrl: 'https://github.com/saamodra/sieve/edit/main/website/',
			},
			sidebar: [
				{
					label: 'Start',
					items: [
						{ label: 'Installation', slug: 'start/installation' },
						{ label: 'Quick start', slug: 'start/quick-start' },
					],
				},
				{
					label: 'Devices',
					items: [
						{ label: 'Overview', slug: 'devices/overview' },
						{ label: 'Android', slug: 'devices/android' },
						{ label: 'iOS Simulator', slug: 'devices/ios-simulator' },
						{ label: 'iPhone', slug: 'devices/iphone' },
					],
				},
				{
					label: 'Guides',
					items: [
						{ label: 'Rules and scripts', slug: 'guides/rules-and-scripts' },
						{ label: 'Certificate pinning', slug: 'guides/certificate-pinning' },
						{ label: 'Cleanup', slug: 'guides/cleanup' },
						{ label: 'Troubleshooting', slug: 'guides/troubleshooting' },
					],
				},
				{
					label: 'Reference',
					items: [
						{ label: 'CLI', slug: 'reference/cli' },
						{ label: 'Responsible use', slug: 'reference/responsible-use' },
					],
				},
			],
		}),
	],
});
