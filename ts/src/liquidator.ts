import { PublicKey } from "@solana/web3.js";
import { CypherUser } from "@chugach-foundation/cypher-client";
import { clientProvider } from "./utils";

export const runLiquidator = async (userAddr: PublicKey) => {
    const user = await CypherUser.loadFromAddr(
        clientProvider.client,
        clientProvider.groupAddr,
        userAddr
    );
    await user.group.loadProxies(clientProvider.client, userAddr);
    // const marginCRatio = user.getCRatio(clientProvider.client);
    // if (marginCRatio < 1.4) {
    //     console.log(
    //         "Found unhealthy margin account with c-ratio ( ",
    //         marginCRatio,
    //         " ): ",
    //         userAddr.toString()
    //     );
    // }
    const liqor = await clientProvider.loadLiqor();
    await liqor.tryLiquidate(user);
};
