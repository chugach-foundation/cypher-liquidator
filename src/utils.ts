require("dotenv").config();
import { Connection } from "@solana/web3.js";
import { Provider } from "@project-serum/anchor";
import { sleep } from "@project-serum/common";
import {
    CypherClient,
    CONFIGS,
    CLUSTER,
    GroupPubkeys,
    CypherGroup,
    CypherLiquidatorController,
    CypherKeeperController,
    CypherUserController,
} from "@chugach-foundation/cypher-client";

class ClientProvider {
    get cluster() {
        return (process.env.CLUSTER as CLUSTER) || "LOCALNET";
    }

    get connection() {
        return new Connection(CONFIGS[this.cluster].RPC_ENDPOINT, {
            commitment: "finalized",
        });
    }

    get cypherPid() {
        return CONFIGS[this.cluster].CYPHER_PID;
    }

    get provider() {
        const provider = Provider.local(CONFIGS[this.cluster].RPC_ENDPOINT);
        return provider;
    }

    get client() {
        const client = new CypherClient(this.cluster, this.provider.wallet);
        return client;
    }

    get groupAddr() {
        return GroupPubkeys[this.cluster];
    }

    async loadGroup() {
        const group = await CypherGroup.load(this.client, this.groupAddr);
        return group;
    }

    async loadLiqor() {
        const user = await CypherUserController.load(this.client, this.groupAddr, "ACCOUNT");
        const liqor = new CypherLiquidatorController(user);
        return liqor;
    }

    async cAssets() {
        const group = await this.loadGroup();
        return group.cAssetMints;
    }

    async getAllUserAddresses() {
        return await CypherGroup.getAllUserAddresses(this.client, this.groupAddr);
    }

    async runCranker() {
        const interval = 3000;
        const keeperController = await CypherKeeperController.load(
            this.client,
            this.groupAddr
        );
        while(true) {
            for (const cAssetMint of (await this.cAssets())) {
                await keeperController.consumeEvents(cAssetMint);
            }
            await sleep(interval);
        }
    }
}

export const clientProvider = new ClientProvider();
