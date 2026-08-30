{{ PROJECT }}-{{ SERVICE }}:
  image: {{ DOCKER_REPO }}/{{ SERVICE }}:{{ VERSION }}
  restart: "always"
  network_mode: "host"
  logging:
    driver: json-file
    options:
      max-size: "10m"
      max-file: "10"
  environment:
    - LOG_LEVEL=Debug
