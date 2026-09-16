FROM alpine:3.20

RUN apk add --no-cache wget ca-certificates

COPY dist/backend-linux /backend

EXPOSE 8080
ENTRYPOINT ["/backend"]
