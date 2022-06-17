FROM node:16-alpine

WORKDIR /usr/src/app

COPY . .

RUN apk add --no-cache openssh-client git
RUN mkdir -p -m 0600 ~/.ssh && ssh-keyscan github.com >> ~/.ssh/known_hosts
RUN --mount=type=ssh yarn install
RUN yarn build

EXPOSE 8080

CMD ["yarn", "start"]
