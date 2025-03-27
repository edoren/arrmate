FROM rust:alpine3.21 AS builder

RUN mkdir /app

RUN apk add --no-cache musl-dev openssl-dev openssl-libs-static pkgconfig curl pipx openjdk11

RUN pipx install openapi-generator-cli
RUN pipx run openapi-generator-cli generate -i "https://raw.githubusercontent.com/Radarr/Radarr/develop/src/Radarr.Api.V3/openapi.json" -g rust -o /app/openapi_generated/radarr --additional-properties=packageName=radarr,library=reqwest-trait,supportAsync=true,useSingleRequestParameter=true,topLevelApiClient=true,useBonBuilder=true --global-property=apis=Queue,models,supportingFiles,apiDocs=false,modelDocs=false
RUN pipx run openapi-generator-cli generate -i "https://raw.githubusercontent.com/Sonarr/Sonarr/develop/src/Sonarr.Api.V3/openapi.json" -g rust -o /app/openapi_generated/sonarr --additional-properties=packageName=sonarr,library=reqwest-trait,supportAsync=true,useSingleRequestParameter=true,topLevelApiClient=true,useBonBuilder=true --global-property=apis=Queue,models,supportingFiles,apiDocs=false,modelDocs=false

COPY ./src /app/src/
COPY ./Cargo.toml ./Cargo.lock /app
WORKDIR /app

RUN cargo build -r

FROM alpine:3.21 AS runner

RUN apk add --no-cache tini bash libgcc

COPY --from=builder /app/target/release/arrmate /arrmate

WORKDIR /config

# Add the cron job
RUN echo '*  *  *  *  * cd /config && /arrmate' >> /etc/crontabs/root

# Run crond  -f for Foreground
ENTRYPOINT ["tini", "--"]
CMD ["/usr/sbin/crond", "-f"]
