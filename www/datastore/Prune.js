Ext.define('pbs-prune-list', {
    extend: 'Ext.data.Model',
    fields: [
	'backup-type',
	'backup-id',
	{
	    name: 'backup-time',
	    type: 'date',
	    dateFormat: 'timestamp',
	},
    ],
});

Ext.define('PBS.DataStorePruneInputPanel', {
    extend: 'Proxmox.panel.InputPanel',
    alias: 'widget.pbsDataStorePruneInputPanel',
    mixins: ['Proxmox.Mixin.CBind'],

    onGetValues: function(values) {
	var me = this;

	values["backup-type"] = me.backup_type;
	values["backup-id"] = me.backup_id;
	return values;
    },

    controller: {
	xclass: 'Ext.app.ViewController',

	init: function(view) {
	    if (!view.url) {
		throw "no url specified";
	    }
	    if (!view.backup_type) {
		throw "no backup_type specified";
	    }
	    if (!view.backup_id) {
		throw "no backup_id specified";
	    }

	    this.reload(); // initial load
	},

	reload: function() {
	    var view = this.getView();

	    let params = view.getValues();
	    params["dry-run"] = true;

	    Proxmox.Utils.API2Request({
		url: view.url,
		method: "POST",
		params: params,
		callback: function() {
		     // for easy breakpoint setting
		},
		failure: function(response, opts) {
		    Ext.Msg.alert(gettext('Error'), response.htmlStatus);
		},
		success: function(response, options) {
		    var data = response.result.data;
		    view.prune_store.setData(data);
		},
	    });
	},

	control: {
	    field: { change: 'reload' },
	},
    },

    column1: [
	{
	    xtype: 'proxmoxintegerfield',
	    name: 'keep-last',
	    allowBlank: true,
	    fieldLabel: gettext('keep-last'),
	    minValue: 1,
	},
	{
	    xtype: 'proxmoxintegerfield',
	    name: 'keep-hourly',
	    allowBlank: true,
	    fieldLabel: gettext('keep-hourly'),
	    minValue: 1,
	},
	{
	    xtype: 'proxmoxintegerfield',
	    name: 'keep-daily',
	    allowBlank: true,
	    fieldLabel: gettext('keep-daily'),
	    minValue: 1,
	},
	{
	    xtype: 'proxmoxintegerfield',
	    name: 'keep-weekly',
	    allowBlank: true,
	    fieldLabel: gettext('keep-weekly'),
	    minValue: 1,
	},
	{
	    xtype: 'proxmoxintegerfield',
	    name: 'keep-monthly',
	    allowBlank: true,
	    fieldLabel: gettext('keep-monthly'),
	    minValue: 1,
	},
	{
	    xtype: 'proxmoxintegerfield',
	    name: 'keep-yearly',
	    allowBlank: true,
	    fieldLabel: gettext('keep-yearly'),
	    minValue: 1,
	},
    ],


    initComponent: function() {
        var me = this;

	me.prune_store = Ext.create('Ext.data.Store', {
	    model: 'pbs-prune-list',
	    sorters: { property: 'backup-time', direction: 'DESC' },
	});

	me.column2 = [
	    {
		xtype: 'grid',
		height: 200,
		store: me.prune_store,
		columns: [
		    {
			header: gettext('Backup Time'),
			sortable: true,
			dataIndex: 'backup-time',
			renderer: function(value, metaData, record) {
			    let text = Ext.Date.format(value, 'Y-m-d H:i:s');
			    if (record.data.keep) {
				return text;
			    } else {
				return '<div style="text-decoration: line-through;">'+ text +'</div>';
			    }
			},
			flex: 1,
		    },
		    {
			text: "keep",
			dataIndex: 'keep',
		    },
		],
	    },
	];

	me.callParent();
    },
});

Ext.define('PBS.DataStorePrune', {
    extend: 'Proxmox.window.Edit',

    method: 'POST',
    submitText: "Prune",

    isCreate: true,

    initComponent: function() {
        var me = this;

	if (!me.datastore) {
	    throw "no datastore specified";
	}
	if (!me.backup_type) {
	    throw "no backup_type specified";
	}
	if (!me.backup_id) {
	    throw "no backup_id specified";
	}

	Ext.apply(me, {
	    url: '/api2/extjs/admin/datastore/' + me.datastore + "/prune",
	    title: "Prune Datastore '" + me.datastore + "'",
	    items: [{
		xtype: 'pbsDataStorePruneInputPanel',
		url: '/api2/extjs/admin/datastore/' + me.datastore + "/prune",
		backup_type: me.backup_type,
		backup_id: me.backup_id,
	    }],
	});

	me.callParent();
    },
});