import mockTransactionData from './mock_data.json';
import { DefaultRpcClient as rpc } from './rpc';

const navigateWithUnknown = async (input: string, navigate: Function) => {
    // feels crude to just search each category for an ID, but works for now
    const addrPromise = rpc
        .getAddressObjects(input)
        .then((data) => {
            navigate(`../addresses/${input}`, { state: data });
        })
        .catch((error) => console.log(error));

    const objInfoPromise = rpc.getObjectInfo(input).then((data) => {
        navigate(`../objects/${input}`, { state: data });
    });

    //if none of the queries find a result, show missing page
    Promise.any([addrPromise, objInfoPromise]).catch((_) =>
        navigate(`../missing/${input}`)
    );
};

const findDataFromID = (targetID: string | undefined, state: any) =>
    state?.category !== undefined
        ? state
        : mockTransactionData.data.find(({ id }) => id === targetID);

export function asciiFromNumberBytes(bytes: number[]) {
    return bytes.map((n) => String.fromCharCode(n)).join('');
}

export function hexToAscii(hex: string) {
    if (!hex || typeof hex != 'string') return;
    hex = hex.replace(/^0x/, '');

    var str = '';
    for (var n = 0; n < hex.length; n += 2)
        str += String.fromCharCode(parseInt(hex.substring(n, 2), 16));

    return str;
}

const stdLibPrefix = /^0x2::/;
export const trimStdLibPrefix = (str: string): string =>
    str.replace(stdLibPrefix, '');

export { findDataFromID, navigateWithUnknown };
