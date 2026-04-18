FROM node:22-alpine

WORKDIR /app

COPY bff/package.json ./
RUN npm install --omit=dev --no-audit --no-fund

COPY bff/dist ./dist

EXPOSE 3000
CMD ["node", "dist/index.js"]
