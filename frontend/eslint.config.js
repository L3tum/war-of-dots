import eslint from "@eslint/js";
import tseslint from "typescript-eslint";

export default tseslint.config(
  // Global ignores (their own object): generated code is out of lint scope
  // entirely — otherwise the json2ts `/* eslint-disable */` banner reports as
  // an unused directive on every regen.
  { ignores: ["dist/**", "node_modules/**", "src/generated/**"] },
  eslint.configs.recommended,
  ...tseslint.configs.recommended,
  {
    languageOptions: {
      globals: {
        document: "readonly",
        window: "readonly",
        console: "readonly",
        setTimeout: "readonly",
        setInterval: "readonly",
        clearTimeout: "readonly",
        clearInterval: "readonly",
        fetch: "readonly",
        WebSocket: "readonly",
        Request: "readonly",
        Response: "readonly",
        Headers: "readonly",
        ArrayBuffer: "readonly",
        Uint8Array: "readonly",
        Int32Array: "readonly",
        Float32Array: "readonly",
        Performance: "readonly",
        performance: "readonly",
        URL: "readonly",
        Blob: "readonly",
        File: "readonly",
        FileReader: "readonly",
        navigator: "readonly",
        localStorage: "readonly",
        sessionStorage: "readonly",
        alert: "readonly",
        confirm: "readonly",
        prompt: "readonly",
        requestAnimationFrame: "readonly",
        cancelAnimationFrame: "readonly",
        addEventListener: "readonly",
        removeEventListener: "readonly",
        dispatchEvent: "readonly",
        location: "readonly",
        history: "readonly",
        matchMedia: "readonly",
        AbortController: "readonly",
        TextDecoder: "readonly",
        TextEncoder: "readonly",
        CustomEvent: "readonly",
        MessageEvent: "readonly",
        Event: "readonly",
        OffscreenCanvas: "readonly",
        ImageBitmap: "readonly",
      },
    },
    rules: {
      "no-unused-vars": "off",
      "@typescript-eslint/no-unused-vars": ["warn", { argsIgnorePattern: "^_" }],
      "@typescript-eslint/no-explicit-any": "warn",
      "@typescript-eslint/no-unused-expressions": "off",
    },
  }
);
