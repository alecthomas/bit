import { createApp } from "./app.js";

const port = Number(process.env.PORT ?? 3000);
const backendUrl = process.env.BACKEND_URL ?? "http://localhost:8080";

const app = createApp({ backendUrl });

app.listen(port, () => {
  console.log(`bff listening on http://localhost:${port}, proxying /api -> ${backendUrl}`);
});
