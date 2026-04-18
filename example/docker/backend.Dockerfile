FROM gcr.io/distroless/base-debian12

COPY dist/backend-linux /backend

EXPOSE 8080
ENTRYPOINT ["/backend"]
