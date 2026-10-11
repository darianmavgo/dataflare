import { useEffect, useRef } from 'react'
import { db } from '../db/db'
import { TableType } from '../db/db-types'
import { TabType, useTabsStore } from './use-store'
import { useTables } from './use-tables'

const MIN_ROWS = 10

export const useAutoOpenTable = () => {
    const { data: tables } = useTables()
    const switchTabTo = useTabsStore((state) => state.switchTabTo)
    const done = useRef(false)

    useEffect(() => {
        if (tables === undefined || done.current) return
        done.current = true
        let cancelled = false
        const run = async () => {
            for (const [schema, items] of Object.entries(tables)) {
                for (const { name, type } of items) {
                    if (type !== TableType.Table) continue
                    const count = await db.tableRowsCount(schema, name, '').catch(() => 0)
                    if (cancelled) return
                    if (count > MIN_ROWS) {
                        if (useTabsStore.getState().activeTab === null) {
                            switchTabTo({
                                type: TabType.Preview,
                                entry: { schema, table: name },
                                tableType: TableType.Table
                            })
                        }
                        return
                    }
                }
            }
        }
        run()
        return () => {
            cancelled = true
        }
    }, [tables])
}
