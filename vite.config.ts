import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// El 5175 evita chocar con TapoController (5173) y con LiveTranscriber (5174).
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    // Explicito a proposito: por defecto Vite puede acabar escuchando solo en
    // ::1, y entonces el devUrl de Tauri (127.0.0.1) recibe un connection
    // refused y la ventana muestra una pagina de error del navegador.
    host: "127.0.0.1",
    port: 5175,
    strictPort: true,
    // Sin esto Vite vigila target/ y se cae con EBUSY en cuanto cargo bloquea
    // una DLL a medio compilar (pasa siempre en el primer `tauri dev`).
    watch: {
      ignored: ["**/target/**", "**/.venv/**", "**/src-tauri/**", "**/sidecar/**"],
    },
  },
  build: {
    target: "esnext",
  },
});
