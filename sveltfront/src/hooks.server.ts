import type { HandleServerError } from '@sveltejs/kit';

export const handleError: HandleServerError = ({ error, event }) => {
	console.error('[SSR ERROR]', event.url.pathname, error);
	return {
		message: String(error instanceof Error ? error.message : error)
	};
};
