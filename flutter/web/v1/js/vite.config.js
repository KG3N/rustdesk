import { defineConfig } from 'vite';

export default defineConfig({
    // KG3N: force libsodium's CJS build so rollup stops failing to resolve
    // ./libsodium.mjs from libsodium-wrappers' ESM entry.
    resolve: {
        alias: {
            'libsodium-wrappers': 'libsodium-wrappers/dist/modules/libsodium-wrappers.js',
            'libsodium': 'libsodium/dist/modules/libsodium.js',
        },
    },
    build: {
        manifest: false,
        rollupOptions: {
            output: {
                entryFileNames: `[name].js`,
                chunkFileNames: `[name].js`,
                assetFileNames: `[name].[ext]`,
            }
        }
    },
})