FROM rust:alpine3.21 AS builder

RUN mkdir /app
WORKDIR /app

COPY ./RadarrAPIV3.json ./SonarrAPIV3.json .

RUN apk add --no-cache musl-dev openssl-dev openssl-libs-static pkgconfig curl pipx openjdk11

RUN pipx install openapi-generator-cli==7.12.0 && \ 
    pipx run openapi-generator-cli==7.12.0 generate -i "$PWD/RadarrAPIV3.json" -g rust -o $PWD/openapi_generated/radarr --additional-properties=packageName=radarr,library=reqwest-trait,supportAsync=true,useSingleRequestParameter=true,topLevelApiClient=true,useBonBuilder=true,enumNameSuffix=Radarr --model-name-prefix=Radarr --global-property=apis=System:Health:Queue,models,supportingFiles,apiDocs=false,modelDocs=false --remove-operation-id-prefix && \
    pipx run openapi-generator-cli==7.12.0 generate -i "$PWD/SonarrAPIV3.json" -g rust -o $PWD/openapi_generated/sonarr --additional-properties=packageName=sonarr,library=reqwest-trait,supportAsync=true,useSingleRequestParameter=true,topLevelApiClient=true,useBonBuilder=true,enumNameSuffix=Sonarr --model-name-prefix=Sonarr --global-property=apis=System:Health:Queue,models,supportingFiles,apiDocs=false,modelDocs=false --remove-operation-id-prefix

COPY ./Cargo.toml ./Cargo.lock .

RUN mkdir -pv src && \
    echo 'fn main() {}' > src/main.rs && \
    cargo build -r && \
    rm -Rvf src

COPY ./src ./src/

RUN touch src/main.rs && cargo build -r

FROM alpine:3.21 AS runner

RUN apk add --no-cache tini bash libgcc

COPY --from=builder /app/target/release/arrmate /arrmate

WORKDIR /config

# Run crond  -f for Foreground
ENTRYPOINT ["tini", "--"]
CMD ["/arrmate"]
