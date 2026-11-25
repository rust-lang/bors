# bors Vue frontend

Vue is currently used on the queue page.

Vite is used to bundle the Vue code into `web/assets/vue`, which is then served by bors as a static page.

## Commands
```bash
npm ci

# Type check and build the frontend
npm run build

# Rebuild on change
# When running bors in the debug profile, it will reload the result from disk on every browser page reload
npm run watch
```
