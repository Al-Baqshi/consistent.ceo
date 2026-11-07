import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';

declare const process: { env: Record<string, string | undefined> };

// Point the dev server at another gateway with GATEWAY_URL=http://127.0.0.1:18877.
const gateway = process.env.GATEWAY_URL ?? 'http://127.0.0.1:18800';

export default defineConfig({
	plugins: [sveltekit()],
	server: {
		proxy: {
			'/api': {
				target: gateway,
				ws: true,
				changeOrigin: true
			},
			'/health': gateway
		}
	}
});
