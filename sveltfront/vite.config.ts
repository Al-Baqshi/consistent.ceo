import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';

export default defineConfig({
	plugins: [sveltekit()],
	server: {
		proxy: {
			'/api': {
				target: 'http://127.0.0.1:18800',
				ws: true,
				changeOrigin: true
			},
			'/health': 'http://127.0.0.1:18800'
		}
	}
});
