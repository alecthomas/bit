FROM node:22-alpine

WORKDIR /app/bff
COPY bff/package.json ./
RUN npm install --omit=dev --no-audit --no-fund

WORKDIR /app
COPY bff/dist ./bff/dist
COPY frontend/dist ./frontend/dist

EXPOSE 3000
CMD ["node", "bff/dist/index.js"]
