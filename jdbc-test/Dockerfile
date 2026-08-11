FROM openjdk:17-slim

WORKDIR /app

RUN apt-get update && apt-get install -y --no-install-recommends wget ca-certificates && \
    wget -q "https://repo1.maven.org/maven2/mysql/mysql-connector-java/8.0.30/mysql-connector-java-8.0.30.jar" -O mysql-connector-java.jar && \
    apt-get purge -y wget && apt-get autoremove -y && rm -rf /var/lib/apt/lists/*

COPY TestJdbc.java .

RUN javac -cp mysql-connector-java.jar TestJdbc.java

CMD ["java", "-cp", ".:mysql-connector-java.jar", "TestJdbc"]
