import { describe, expect, it } from "vitest";
import request from "supertest";
import { createApp } from "./app.js";
import os from "node:os";

describe("bff", () => {
  it("responds to /healthz", async () => {
    const app = createApp({
      backendUrl: "http://unused.example",
      staticDir: os.tmpdir(),
    });
    const res = await request(app).get("/healthz");
    expect(res.status).toBe(200);
    expect(res.body).toEqual({ ok: true });
  });
});
