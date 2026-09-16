import express, { type Express } from "express";
import { createProxyMiddleware } from "http-proxy-middleware";
import path from "node:path";
import { fileURLToPath } from "node:url";

export interface AppOptions {
  backendUrl: string;
  staticDir?: string;
}

export function createApp(opts: AppOptions): Express {
  const app = express();

  app.get("/healthz", (_req, res) => {
    res.json({ ok: true });
  });

  app.use(
    "/api",
    createProxyMiddleware({
      target: opts.backendUrl,
      changeOrigin: true,
      pathRewrite: { "^/api": "" },
    }),
  );

  const staticDir =
    opts.staticDir ??
    path.resolve(
      path.dirname(fileURLToPath(import.meta.url)),
      "../../frontend/dist",
    );
  app.use(express.static(staticDir));

  return app;
}
